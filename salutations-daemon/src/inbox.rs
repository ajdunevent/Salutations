//! New mail in each account's INBOX: the account's rules run on it, then
//! what is left unread is notified about. Also the unread badge and the
//! notifications' buttons.

use std::rc::Rc;
use std::time::{Duration, Instant};

use glib::prelude::*;
use salutations_core::camel::{self, prelude::*};
use salutations_core::debug::log;
use salutations_core::engine::AccountState;
use salutations_core::rules::{self, Dmarc, Facts, Filing, Rule};
use salutations_core::{Address, Settings};

use crate::daemon::{DELETED, Daemon, FLAGGED, Handoff, JUNK, SEEN, message_key};
use crate::notify::{self, Action, MessageNote};

/// How long rules that ask about contacts wait for the address books at
/// startup before taking every sender as unknown.
const CONTACTS_WAIT: Duration = Duration::from_secs(10);

fn notify_state_path() -> std::path::PathBuf {
    salutations_core::data_dir().join("notify-state.ini")
}

fn load_last_uid(account: &str) -> Option<u64> {
    let kf = glib::KeyFile::new();
    kf.load_from_file(notify_state_path(), glib::KeyFileFlags::NONE).ok()?;
    kf.uint64(account, "last-uid").ok()
}

fn save_last_uid(account: &str, uid: u64) {
    let kf = glib::KeyFile::new();
    let path = notify_state_path();
    let _ = kf.load_from_file(&path, glib::KeyFileFlags::NONE);
    kf.set_uint64(account, "last-uid", uid);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = kf.save_to_file(&path) {
        glib::g_warning!("salutations", "Cannot save notification state: {}", e);
    }
}

/// The addresses in a summary field such as "Ann <ann@x.org>, bob@y.org".
pub(crate) fn address_list(raw: &str) -> Vec<Address> {
    let list = camel::InternetAddress::new();
    let n = list.unformat(raw);
    (0..n.max(0))
        .filter_map(|i| list.get(i))
        .filter_map(|(name, email)| {
            Some(Address { name: name.map(|n| n.to_string()).filter(|n| !n.is_empty()), email: email?.to_string() })
        })
        .collect()
}

impl Daemon {
    /// Starts watching the account's INBOX, once per session, and notifies
    /// about mail that arrived since the last run.
    pub(crate) fn watch_inbox(self: &Rc<Self>, acct: &Rc<AccountState>) {
        let Some(folder) = acct.inbox() else { return };
        let extra = self.extra(acct.uid());
        if extra.watched.replace(true) {
            return;
        }
        let uids: Vec<String> = folder.uids().iter().map(|u| u.to_string()).collect();
        if extra.last_uid.get().is_none() {
            // The first run ever starts from what is there, without
            // notifying about all of it.
            let saved = load_last_uid(acct.uid());
            let start = saved.unwrap_or_else(|| uids.iter().filter_map(|u| u.parse().ok()).max().unwrap_or(0));
            extra.last_uid.set(Some(start));
            if saved.is_none() {
                save_last_uid(acct.uid(), start);
            }
        }
        let refs = glib::thread_guard::ThreadGuard::new((Rc::downgrade(self), Rc::downgrade(acct)));
        folder.connect_changed(move |folder, changes| {
            let (Some(this), Some(acct)) = (refs.get_ref().0.upgrade(), refs.get_ref().1.upgrade()) else { return };
            this.inbox_changed(&acct, folder, changes);
        });
        self.notify_new(acct, &folder, &uids);
        self.update_badge();
    }

    fn inbox_changed(self: &Rc<Self>, acct: &Rc<AccountState>, folder: &camel::Folder, changes: &camel::FolderChanges) {
        let extra = self.extra(acct.uid());
        let uid = acct.uid().to_string();
        // Read, moved, or deleted anywhere: its notification goes.
        let gone = |u: &String| changes.removed.contains(u) || folder.message_info(u).is_none() || folder.message_flags(u) & (SEEN | DELETED) != 0;
        let withdrawn: Vec<String> = extra.noted.borrow().iter().filter(|u| gone(u)).cloned().collect();
        for u in &withdrawn {
            self.notifier.withdraw(&notify::Subject::Message { account: uid.clone(), uid: u.clone() });
        }
        extra.noted.borrow_mut().retain(|u| !withdrawn.contains(u));
        let before = extra.combined.borrow().len();
        extra.combined.borrow_mut().retain(|u| !gone(u));
        let after = extra.combined.borrow().len();
        if after != before {
            if after == 0 {
                self.notifier.withdraw(&notify::Subject::Combined { account: uid.clone() });
            } else {
                self.show_combined(acct, folder, false);
            }
        }
        self.notify_new(acct, folder, &changes.added);
        self.update_badge();
    }

    /// Runs the account's rules on messages among `uids` newer than any
    /// considered before, then notifies about those still unread in INBOX.
    fn notify_new(self: &Rc<Self>, acct: &Rc<AccountState>, folder: &camel::Folder, uids: &[String]) {
        let extra = self.extra(acct.uid());
        let last = extra.last_uid.get().unwrap_or(0);
        log(&format!("{}: {} INBOX arrivals, last notified {last}", acct.uid(), uids.len()));
        let mut fresh: Vec<(u64, String)> =
            uids.iter().filter_map(|u| u.parse::<u64>().ok().map(|n| (n, u.clone()))).filter(|(n, _)| *n > last).collect();
        if fresh.is_empty() {
            return;
        }
        fresh.sort();
        let top = fresh.last().map_or(last, |(n, _)| *n);
        extra.last_uid.set(Some(top));
        save_last_uid(acct.uid(), top);
        let fresh: Vec<String> = {
            let mut expected = extra.expected.borrow_mut();
            fresh
                .into_iter()
                .map(|(_, u)| u)
                .filter(|u| {
                    // A message Salutations moved here is not new mail.
                    let Some(k) = message_key(folder, u) else { return false };
                    match expected.iter().position(|e| *e == k) {
                        Some(i) => {
                            expected.swap_remove(i);
                            false
                        }
                        None => true,
                    }
                })
                .collect()
        };
        if fresh.is_empty() {
            return;
        }
        let rules: Vec<Rule> =
            rules::load(acct.uid()).into_iter().filter(|r| !r.conditions.is_empty() && r.does_anything()).collect();
        let (this, acct, folder) = (self.clone(), acct.clone(), folder.clone());
        glib::spawn_future_local(async move {
            let kept = if rules.is_empty() { fresh } else { this.run_rules(&acct, &folder, fresh, &rules).await.0 };
            this.notify_fresh(&acct, &folder, kept);
        });
    }

    /// Notifies about the messages among `fresh`, new mail the rules left
    /// in INBOX, that are unread.
    fn notify_fresh(self: &Rc<Self>, acct: &Rc<AccountState>, folder: &camel::Folder, fresh: Vec<String>) {
        let fresh: Vec<String> = fresh.into_iter().filter(|u| folder.message_flags(u) & (SEEN | DELETED | JUNK) == 0).collect();
        log(&format!("{}: {} to notify about", acct.uid(), fresh.len()));
        if fresh.is_empty() {
            return;
        }
        // Nothing while the user is looking at this INBOX.
        let shown = self.shown.borrow().clone();
        if shown.active && shown.account == acct.uid() && shown.folder.eq_ignore_ascii_case("INBOX") {
            return;
        }
        let extra = self.extra(acct.uid());
        let account = acct.uid().to_string();
        let sound = !Settings::get().silent_accounts.contains(&account);
        let total = extra.noted.borrow().len() + fresh.len();
        let limit = Settings::get().notification_limit;
        if !extra.combined.borrow().is_empty() || limit.is_some_and(|n| total > n as usize) {
            let noted: Vec<String> = extra.noted.borrow_mut().drain(..).collect();
            for u in &noted {
                self.notifier.withdraw(&notify::Subject::Message { account: account.clone(), uid: u.clone() });
            }
            extra.combined.borrow_mut().extend(noted);
            extra.combined.borrow_mut().extend(fresh);
            self.show_combined(acct, folder, sound);
            return;
        }
        let junk = acct.special().is_some_and(|s| s.junk.is_some());
        let account_name = (self.engine.accounts().len() > 1).then(|| acct.account.display_name().to_string());
        for u in fresh {
            let Some(info) = folder.message_info(&u) else { continue };
            let from = address_list(&info.from().unwrap_or_default()).into_iter().next().unwrap_or_default();
            let subject = info.subject().map(|s| s.to_string()).unwrap_or_default();
            let preview = info.preview().map(|p| salutations_core::render::clean_preview(&p)).unwrap_or_default();
            let verified =
                info.user_header("Authentication-Results").and_then(|h| salutations_core::verify::parse(&h).verified_domain(&from.email));
            extra.noted.borrow_mut().push(u.clone());
            // The contact's photo can take a moment to fetch.
            let (this, account, account_name) = (self.clone(), account.clone(), account_name.clone());
            glib::spawn_future_local(async move {
                let (contact_name, photo) = this.people.lookup(&from.email, verified.as_deref()).await;
                this.notifier.message(MessageNote {
                    account: &account,
                    account_name,
                    uid: &u,
                    sender: from.display(),
                    contact_name,
                    photo,
                    subject: &subject,
                    preview: &preview,
                    sound,
                    junk,
                });
            });
        }
    }

    fn show_combined(&self, acct: &AccountState, folder: &camel::Folder, sound: bool) {
        let combined = self.extra(acct.uid()).combined.borrow().clone();
        let senders: Vec<String> = combined
            .iter()
            .rev()
            .filter_map(|u| folder.message_info(u))
            .filter_map(|i| address_list(&i.from().unwrap_or_default()).into_iter().next())
            .map(|a| a.display().to_string())
            .collect();
        self.notifier.combined(acct.uid(), &acct.account.display_name(), combined.len(), &senders, sound);
    }

    /// The unread count of every account's INBOX. An account whose INBOX
    /// is unavailable counts with its last known number.
    pub(crate) fn update_badge(&self) {
        let mut total = 0;
        for a in self.engine.accounts() {
            let extra = self.extra(a.uid());
            if let Some(n) = a.inbox().and_then(|f| f.folder_summary()).map(|s| s.unread_count() as i64) {
                extra.unread.set(Some(n));
            }
            total += extra.unread.get().unwrap_or(0);
        }
        self.notifier.set_badge(total);
    }

    pub(crate) fn notification_action(self: &Rc<Self>, action: Action) {
        let (kind, account, uid) = match &action {
            Action::Open { account, uid } => ("open", account.clone(), uid.clone()),
            Action::Reply { account, uid } => ("reply", account.clone(), uid.clone()),
            Action::OpenInbox { account } => ("open-inbox", account.clone(), String::new()),
            Action::Delete { account, uid } => ("delete", account.clone(), uid.clone()),
            Action::Junk { account, uid } => ("junk", account.clone(), uid.clone()),
        };
        let Some(acct) = self.engine.account(&account) else { return };
        let special = acct.special().unwrap_or_default();
        // The window's undo list takes them while it is open; deleting for
        // good asks first, which needs the window.
        let direct = match kind {
            "delete" => !self.window_open() && special.trash.is_some(),
            "junk" => !self.window_open() && special.junk.is_some(),
            _ => false,
        };
        if !direct {
            if kind != "junk" && (kind != "delete" || special.trash.is_none()) {
                self.show_window();
            }
            self.emit("notification", (kind.to_string(), account, uid).to_variant());
            return;
        }
        let Some(inbox) = acct.inbox() else { return };
        let settings = Settings::get();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let uids = vec![uid];
            let dest = if kind == "junk" {
                mark_junk(&inbox, &uids, true);
                if settings.read_on_junk {
                    set_flags(&inbox, &uids, SEEN, SEEN);
                }
                special.junk
            } else {
                if settings.read_on_delete {
                    set_flags(&inbox, &uids, SEEN, SEEN);
                }
                special.trash
            };
            if let Some(dest) = dest {
                this.move_now(&acct, &inbox, &uids, &dest).await;
            }
        });
    }

    /// Moves `uids` to the account's folder `dest` without undo.
    pub(crate) async fn move_now(self: &Rc<Self>, acct: &AccountState, folder: &camel::Folder, uids: &[String], dest: &str) -> bool {
        let to = match self.folder(acct.uid(), dest).await {
            Ok(f) => f,
            Err(e) => {
                log(&format!("{}: cannot open {dest}: {}", acct.uid(), e.message()));
                return false;
            }
        };
        let job = Handoff((folder.clone(), to, uids.to_vec()));
        let result = gio::spawn_blocking(move || {
            let Handoff((src, to, list)) = job;
            src.transfer_messages_to_sync(&list, &to, true, None::<&gio::Cancellable>).map(|_| ())
        })
        .await;
        match result {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                log(&format!("{}: move to {dest} failed: {}", acct.uid(), e.message()));
                false
            }
            Err(_) => false,
        }
    }

    /// What the rules look at in one message; None when it is gone.
    async fn facts(&self, folder: &camel::Folder, uid: &str, headers: bool) -> Option<Facts> {
        const ATTACHMENTS: u32 = camel::MessageFlags::ATTACHMENTS.bits();
        let info = folder.message_info(uid)?;
        let from = address_list(&info.from().unwrap_or_default());
        let mut to_cc = address_list(&info.to().unwrap_or_default());
        to_cc.extend(address_list(&info.cc().unwrap_or_default()));
        let email = from.first().map(|a| a.email.clone()).unwrap_or_default();
        let dmarc = info.user_header("Authentication-Results").map(|h| {
            let v = salutations_core::verify::parse(&h);
            if v.verified_domain(&email).is_some() {
                Dmarc::Pass
            } else if v.failed() {
                Dmarc::Fail
            } else {
                Dmarc::None
            }
        });
        let mut facts = Facts {
            subject: info.subject().map(|s| s.to_string()).unwrap_or_default(),
            attachments: info.flags() & ATTACHMENTS != 0,
            dmarc,
            in_contacts: self.people.known(&email).unwrap_or(false),
            from,
            to_cc,
            headers: Vec::new(),
        };
        if headers {
            match folder.message_future(uid, glib::ffi::G_PRIORITY_DEFAULT).await {
                Ok(Some(msg)) => facts.headers = salutations_core::render::headers(&msg),
                Ok(None) => {}
                Err(e) => log(&format!("rules: headers of {uid} not read: {e}")),
            }
        }
        Some(facts)
    }

    /// Runs `rules` on `uids` in `folder`, the account's INBOX: flags at
    /// once, then moves without undo. Returns the messages left in INBOX
    /// and how many matched a rule.
    pub(crate) async fn run_rules(
        self: &Rc<Self>,
        acct: &AccountState,
        folder: &camel::Folder,
        uids: Vec<String>,
        rules: &[Rule],
    ) -> (Vec<String>, usize) {
        let headers = rules.iter().any(Rule::needs_headers);
        if rules.iter().any(Rule::needs_contacts) {
            let start = Instant::now();
            while self.people.known("").is_none() && start.elapsed() < CONTACTS_WAIT {
                glib::timeout_future(Duration::from_millis(250)).await;
            }
        }
        let mut kept = Vec::new();
        let mut filed: Vec<(Filing, Vec<String>)> = Vec::new();
        let mut matched = 0;
        for uid in uids {
            let Some(facts) = self.facts(folder, &uid, headers).await else { continue };
            let out = rules::apply(rules, &facts);
            if out.is_empty() {
                kept.push(uid);
                continue;
            }
            matched += 1;
            let (mut mask, mut set) = (0, 0);
            if let Some(read) = out.read {
                mask |= SEEN;
                set |= if read { SEEN } else { 0 };
            }
            if let Some(star) = out.star {
                mask |= FLAGGED;
                set |= if star { FLAGGED } else { 0 };
            }
            if mask != 0 {
                folder.set_message_flags(&uid, mask, set);
            }
            match out.file {
                Some(f) => match filed.iter_mut().find(|(g, _)| *g == f) {
                    Some((_, list)) => list.push(uid),
                    None => filed.push((f, vec![uid])),
                },
                None => kept.push(uid),
            }
        }
        for (filing, uids) in filed {
            if !self.file_by_rule(acct, folder, &filing, &uids).await {
                kept.extend(uids);
            }
        }
        if matched > 0 {
            log(&format!("rules: {matched} messages matched in {}", acct.uid()));
        }
        (kept, matched)
    }

    /// Makes the move a rule asks for; false when there is nowhere to go.
    async fn file_by_rule(self: &Rc<Self>, acct: &AccountState, folder: &camel::Folder, filing: &Filing, uids: &[String]) -> bool {
        let special = acct.special().unwrap_or_default();
        let dest = match filing {
            Filing::Folder(f) => Some(f.clone()),
            Filing::Archive => special.archive,
            Filing::Junk => special.junk,
            Filing::Trash => special.trash,
        };
        let here = folder.full_name().map(|n| n.to_string());
        let Some(dest) = dest.filter(|d| Some(d) != here.as_ref()) else {
            log(&format!("rules: nowhere to file {filing:?}"));
            return false;
        };
        let settings = Settings::get();
        match filing {
            Filing::Junk => {
                mark_junk(folder, uids, true);
                if settings.read_on_junk {
                    set_flags(folder, uids, SEEN, SEEN);
                }
            }
            Filing::Trash if settings.read_on_delete => set_flags(folder, uids, SEEN, SEEN),
            _ => {}
        }
        self.move_now(acct, folder, uids, &dest).await
    }

    /// Runs `rule` once on every message in the account's INBOX; how many
    /// it matched.
    pub(crate) async fn apply_rule_now(self: &Rc<Self>, acct: &Rc<AccountState>, rule: Rule) -> Result<usize, glib::Error> {
        let Some(folder) = acct.inbox() else {
            return Err(glib::Error::new(gio::IOErrorEnum::NotFound, "Inbox isn't available yet; the rule will run on new mail"));
        };
        let uids: Vec<String> =
            folder.uids().iter().map(|u| u.to_string()).filter(|u| folder.message_flags(u) & DELETED == 0).collect();
        let (_, matched) = self.run_rules(acct, &folder, uids, std::slice::from_ref(&rule)).await;
        Ok(matched)
    }
}

pub(crate) fn set_flags(folder: &camel::Folder, uids: &[String], mask: u32, set: u32) {
    folder.freeze();
    for uid in uids {
        folder.set_message_flags(uid, mask, set);
    }
    folder.thaw();
}

pub(crate) fn mark_junk(folder: &camel::Folder, uids: &[String], junk: bool) {
    const NOTJUNK: u32 = camel::MessageFlags::NOTJUNK.bits();
    set_flags(folder, uids, JUNK | NOTJUNK, if junk { JUNK } else { NOTJUNK });
    for uid in uids {
        if let Some(info) = folder.message_info(uid) {
            camel::message_info_set_user_flag(&info, "$Junk", junk);
            camel::message_info_set_user_flag(&info, "$NotJunk", !junk);
        }
    }
}
