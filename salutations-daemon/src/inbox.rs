//! New mail in the folders each account watches: INBOX, the folders it
//! notifies for, and those its rules run on. The rules run on new mail,
//! then what is left unread is notified about where that is asked for.
//! Also the unread badge and the notifications' buttons.

use std::cell::{Cell, RefCell};
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

pub(crate) struct RulesRun {
    /// Left in the folder.
    kept: Vec<String>,
    matched: usize,
    /// Waiting for headers that could not be read.
    waiting: Vec<String>,
}

/// How long rules that ask about contacts wait for the address books at
/// startup before taking every sender as unknown.
const CONTACTS_WAIT: Duration = Duration::from_secs(10);
/// When a rule needs a message's headers and it could not be downloaded,
/// the message waits and is tried again after each of these; after the
/// last, its rules run without the headers.
const HEADER_RETRIES: [Duration; 3] = [Duration::from_secs(30), Duration::from_secs(120), Duration::from_secs(300)];

/// A message as the rules see it.
enum Looked {
    Gone,
    /// Its headers were needed but could not be read.
    NoHeaders(Facts),
    Ready(Facts),
}

/// A folder watched for new mail.
pub(crate) struct Watch {
    pub path: String,
    pub folder: camel::Folder,
    /// The highest UID considered.
    last_uid: Cell<u64>,
    /// The unread count last sent to the window.
    unread: Cell<Option<u32>>,
    /// Messages with a notification of their own.
    pub noted: RefCell<Vec<String>>,
    handler: RefCell<Option<glib::SignalHandlerId>>,
}

fn notify_state_path() -> std::path::PathBuf {
    salutations_core::data_dir().join("notify-state.ini")
}

/// INBOX keeps the key it had before other folders were watched.
fn state_key(folder: &str) -> String {
    if folder == "INBOX" {
        "last-uid".to_string()
    } else {
        format!("last-uid:{}", glib::Uri::escape_string(folder, None, false))
    }
}

fn load_last_uid(account: &str, folder: &str) -> Option<u64> {
    let kf = glib::KeyFile::new();
    kf.load_from_file(notify_state_path(), glib::KeyFileFlags::NONE).ok()?;
    kf.uint64(account, &state_key(folder)).ok()
}

/// Saves the highest UID considered; None forgets it.
fn save_last_uid(account: &str, folder: &str, uid: Option<u64>) {
    let kf = glib::KeyFile::new();
    let path = notify_state_path();
    let _ = kf.load_from_file(&path, glib::KeyFileFlags::NONE);
    match uid {
        Some(uid) => kf.set_uint64(account, &state_key(folder), uid),
        None => {
            let _ = kf.remove_key(account, &state_key(folder));
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = kf.save_to_file(&path) {
        glib::g_warning!("salutations", "Cannot save notification state: {}", e);
    }
}

/// Moves the saved state of `old` and its subfolders to their new paths.
pub(crate) fn rename_state(account: &str, old: &str, new: &str) {
    let kf = glib::KeyFile::new();
    let path = notify_state_path();
    if kf.load_from_file(&path, glib::KeyFileFlags::NONE).is_err() {
        return;
    }
    let Ok(keys) = kf.keys(account) else { return };
    for key in keys.iter() {
        let Some(folder) = key.strip_prefix("last-uid:").and_then(|f| glib::Uri::unescape_string(f, None::<&str>)) else { continue };
        let Some(renamed) = salutations_core::renamed_path(&folder, old, new) else { continue };
        if let Ok(v) = kf.uint64(account, key) {
            let _ = kf.remove_key(account, key);
            kf.set_uint64(account, &state_key(&renamed), v);
        }
    }
    let _ = kf.save_to_file(&path);
}

fn max_uid(folder: &camel::Folder) -> u64 {
    folder.uids().iter().filter_map(|u| u.parse().ok()).max().unwrap_or(0)
}

/// The rules that run, in order.
fn active_rules(account: &str) -> Vec<Rule> {
    rules::load(account).into_iter().filter(|r| !r.conditions.is_empty() && r.does_anything()).collect()
}

/// A folder's path as shown: "Inbox › Lists" for "INBOX/Lists".
pub(crate) fn shown_path(path: &str) -> String {
    path.split('/')
        .enumerate()
        .map(|(i, p)| if i == 0 && p.eq_ignore_ascii_case("INBOX") { "Inbox" } else { p })
        .collect::<Vec<_>>()
        .join(" › ")
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
    /// The folders the account watches: INBOX, those it notifies for, and
    /// those its rules run on.
    pub(crate) fn wanted_folders(&self, account: &str) -> Vec<String> {
        let mut out = vec!["INBOX".to_string()];
        let more = Settings::get().notify_folders_for(account).into_iter().chain(active_rules(account).into_iter().flat_map(|r| r.folders()));
        for f in more {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&f)) {
                out.push(f);
            }
        }
        out
    }

    pub(crate) fn watch(&self, account: &str, folder: &str) -> Option<Rc<Watch>> {
        self.extra(account).watches.borrow().iter().find(|(p, _)| p.eq_ignore_ascii_case(folder)).map(|(_, w)| w.clone())
    }

    /// Starts and stops watching folders to match what the account wants.
    /// A folder watched for the first time starts from what it holds,
    /// without notifying about it; one watched before catches up on what
    /// arrived since. Folders that cannot be opened yet are tried again
    /// after the next sync.
    pub(crate) fn update_watches(self: &Rc<Self>, acct: &Rc<AccountState>) {
        let Some(inbox) = acct.inbox() else { return };
        let extra = self.extra(acct.uid());
        let wanted = self.wanted_folders(acct.uid());
        let gone: Vec<String> = extra
            .watches
            .borrow()
            .keys()
            .filter(|p| !wanted.iter().any(|w| w.eq_ignore_ascii_case(p)))
            .cloned()
            .collect();
        for path in gone {
            self.unwatch(acct, &path, true);
        }
        for path in wanted {
            if self.watch(acct.uid(), &path).is_some() || !extra.opening.borrow_mut().insert(path.clone()) {
                continue;
            }
            let (this, acct, inbox) = (self.clone(), acct.clone(), inbox.clone());
            glib::spawn_future_local(async move {
                let folder = if path == "INBOX" {
                    Ok(inbox)
                } else {
                    let opened = this.folder(acct.uid(), &path).await;
                    match opened {
                        Ok(f) if load_last_uid(acct.uid(), &path).is_none() => {
                            // A folder never opened has no summary yet; its
                            // starting point must come from the server.
                            f.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await.map(|_| f)
                        }
                        other => other,
                    }
                };
                this.extra(acct.uid()).opening.borrow_mut().remove(&path);
                match folder {
                    Ok(f) => this.start_watch(&acct, &path, &f),
                    Err(e) => log(&format!("{}: cannot watch {path}: {}", acct.uid(), e.message())),
                }
            });
        }
    }

    fn start_watch(self: &Rc<Self>, acct: &Rc<AccountState>, path: &str, folder: &camel::Folder) {
        if self.watch(acct.uid(), path).is_some() || !self.wanted_folders(acct.uid()).iter().any(|w| w.eq_ignore_ascii_case(path)) {
            return;
        }
        let uids: Vec<String> = folder.uids().iter().map(|u| u.to_string()).collect();
        let saved = load_last_uid(acct.uid(), path);
        let start = saved.unwrap_or_else(|| max_uid(folder));
        if saved.is_none() {
            save_last_uid(acct.uid(), path, Some(start));
        }
        let watch = Rc::new(Watch {
            path: path.to_string(),
            folder: folder.clone(),
            last_uid: Cell::new(start),
            unread: Cell::new(None),
            noted: RefCell::default(),
            handler: RefCell::default(),
        });
        let refs = glib::thread_guard::ThreadGuard::new((Rc::downgrade(self), Rc::downgrade(acct), Rc::downgrade(&watch)));
        let handler = folder.connect_changed(move |folder, changes| {
            let r = refs.get_ref();
            let (Some(this), Some(acct), Some(watch)) = (r.0.upgrade(), r.1.upgrade(), r.2.upgrade()) else { return };
            this.folder_changed(&acct, &watch, folder, changes);
        });
        *watch.handler.borrow_mut() = Some(handler);
        self.extra(acct.uid()).watches.borrow_mut().insert(path.to_string(), watch.clone());
        log(&format!("{}: watching {path} from UID {start}", acct.uid()));
        self.send_unread(acct, &watch);
        self.notify_new(acct, &watch, &uids);
        self.update_badge();
    }

    /// Stops watching `path`; with `forget`, a later watch starts afresh.
    pub(crate) fn unwatch(&self, acct: &AccountState, path: &str, forget: bool) {
        let Some(watch) = self.extra(acct.uid()).watches.borrow_mut().remove(path) else { return };
        if let Some(h) = watch.handler.borrow_mut().take() {
            watch.folder.disconnect(h);
        }
        for u in watch.noted.borrow().iter() {
            self.notifier.withdraw(&notify::Subject::Message { account: acct.uid().to_string(), folder: path.to_string(), uid: u.clone() });
        }
        if forget {
            save_last_uid(acct.uid(), path, None);
        }
    }

    /// Refreshes the watched folders other than INBOX, which the account's
    /// sync covers; new mail in them arrives through their change signal.
    pub(crate) fn refresh_watched(self: &Rc<Self>, acct: &Rc<AccountState>) {
        self.update_watches(acct);
        let folders: Vec<camel::Folder> =
            self.extra(acct.uid()).watches.borrow().values().filter(|w| w.path != "INBOX").map(|w| w.folder.clone()).collect();
        for f in folders {
            glib::spawn_future_local(async move {
                let _ = f.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await;
            });
        }
    }

    fn folder_changed(self: &Rc<Self>, acct: &Rc<AccountState>, watch: &Rc<Watch>, folder: &camel::Folder, changes: &camel::FolderChanges) {
        let extra = self.extra(acct.uid());
        let uid = acct.uid().to_string();
        // Read, moved, or deleted anywhere: its notification goes.
        let gone = |u: &String| changes.removed.contains(u) || folder.message_info(u).is_none() || folder.message_flags(u) & (SEEN | DELETED) != 0;
        let withdrawn: Vec<String> = watch.noted.borrow().iter().filter(|u| gone(u)).cloned().collect();
        for u in &withdrawn {
            self.notifier.withdraw(&notify::Subject::Message { account: uid.clone(), folder: watch.path.clone(), uid: u.clone() });
        }
        watch.noted.borrow_mut().retain(|u| !withdrawn.contains(u));
        let before = extra.combined.borrow().len();
        extra.combined.borrow_mut().retain(|(p, u)| *p != watch.path || !gone(u));
        let after = extra.combined.borrow().len();
        if after != before {
            if after == 0 {
                self.notifier.withdraw(&notify::Subject::Combined { account: uid.clone() });
            } else {
                self.show_combined(acct, false);
            }
        }
        self.send_unread(acct, watch);
        self.notify_new(acct, watch, &changes.added);
        self.update_badge();
        self.update_status();
    }

    /// Keeps the window's folder tree current for a watched folder, which
    /// otherwise learns counts only when the folders are listed.
    fn send_unread(&self, acct: &AccountState, watch: &Watch) {
        let Some(n) = watch.folder.folder_summary().map(|s| s.unread_count()) else { return };
        if watch.unread.replace(Some(n)) != Some(n) {
            self.emit("unread", (acct.uid().to_string(), watch.path.clone(), n as i32).to_variant());
        }
    }

    /// Runs the rules for the folder on messages among `uids` newer than
    /// any considered before, then notifies about those still there and
    /// unread, when the folder notifies.
    fn notify_new(self: &Rc<Self>, acct: &Rc<AccountState>, watch: &Rc<Watch>, uids: &[String]) {
        let extra = self.extra(acct.uid());
        let folder = &watch.folder;
        let last = watch.last_uid.get();
        log(&format!("{}: {} {} arrivals, last considered {last}", acct.uid(), uids.len(), watch.path));
        let mut fresh: Vec<(u64, String)> =
            uids.iter().filter_map(|u| u.parse::<u64>().ok().map(|n| (n, u.clone()))).filter(|(n, _)| *n > last).collect();
        if fresh.is_empty() {
            return;
        }
        fresh.sort();
        let top = fresh.last().map_or(last, |(n, _)| *n);
        watch.last_uid.set(top);
        save_last_uid(acct.uid(), &watch.path, Some(top));
        let fresh: Vec<String> = {
            let mut expected = extra.expected.borrow_mut();
            fresh
                .into_iter()
                .map(|(_, u)| u)
                .filter(|u| {
                    // A message Salutations moved here is not new mail.
                    let Some(k) = message_key(folder, u) else { return false };
                    match expected.iter().position(|(p, e)| p.eq_ignore_ascii_case(&watch.path) && *e == k) {
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
        let path = watch.path.clone();
        let rules: Vec<Rule> = active_rules(acct.uid()).into_iter().filter(|r| r.runs_in(&path)).collect();
        let (this, acct, watch) = (self.clone(), acct.clone(), watch.clone());
        glib::spawn_future_local(async move {
            let folder = watch.folder.clone();
            if rules.is_empty() {
                this.notify_fresh(&acct, &watch, fresh);
                return;
            }
            let ran = this.run_rules(&acct, &folder, fresh, &rules, true).await;
            this.notify_fresh(&acct, &watch, ran.kept);
            let mut waiting = ran.waiting;
            for (n, wait) in HEADER_RETRIES.iter().enumerate() {
                if waiting.is_empty() {
                    return;
                }
                glib::timeout_future(*wait).await;
                waiting.retain(|u| folder.message_info(u).is_some());
                let rules: Vec<Rule> = active_rules(acct.uid()).into_iter().filter(|r| r.runs_in(&path)).collect();
                let last = n + 1 == HEADER_RETRIES.len();
                let ran = this.run_rules(&acct, &folder, std::mem::take(&mut waiting), &rules, !last).await;
                this.notify_fresh(&acct, &watch, ran.kept);
                waiting = ran.waiting;
            }
        });
    }

    /// Notifies about the messages among `fresh`, new mail the rules left
    /// in the folder, that are unread, when the folder notifies and is not
    /// hidden.
    fn notify_fresh(self: &Rc<Self>, acct: &Rc<AccountState>, watch: &Rc<Watch>, fresh: Vec<String>) {
        let folder = &watch.folder;
        let notifies = Settings::get().notify_folders_for(acct.uid()).iter().any(|f| f.eq_ignore_ascii_case(&watch.path))
            && !salutations_core::hidden::is_hidden(acct.uid(), &watch.path);
        let fresh: Vec<String> = fresh.into_iter().filter(|u| folder.message_flags(u) & (SEEN | DELETED | JUNK) == 0).collect();
        log(&format!("{}: {} to notify about in {}", acct.uid(), if notifies { fresh.len() } else { 0 }, watch.path));
        if fresh.is_empty() || !notifies {
            return;
        }
        // Nothing while the user is looking at this folder.
        let shown = self.shown.borrow().clone();
        if shown.active && shown.account == acct.uid() && shown.folder.eq_ignore_ascii_case(&watch.path) {
            return;
        }
        let extra = self.extra(acct.uid());
        let account = acct.uid().to_string();
        let sound = !Settings::get().silent_accounts.contains(&account);
        let noted: usize = extra.watches.borrow().values().map(|w| w.noted.borrow().len()).sum();
        let total = noted + fresh.len();
        let limit = Settings::get().notification_limit;
        if !extra.combined.borrow().is_empty() || limit.is_some_and(|n| total > n as usize) {
            let watches: Vec<Rc<Watch>> = extra.watches.borrow().values().cloned().collect();
            for w in watches {
                let noted: Vec<String> = w.noted.borrow_mut().drain(..).collect();
                for u in &noted {
                    self.notifier.withdraw(&notify::Subject::Message { account: account.clone(), folder: w.path.clone(), uid: u.clone() });
                }
                extra.combined.borrow_mut().extend(noted.into_iter().map(|u| (w.path.clone(), u)));
            }
            extra.combined.borrow_mut().extend(fresh.into_iter().map(|u| (watch.path.clone(), u)));
            self.show_combined(acct, sound);
            return;
        }
        let junk = acct.special().is_some_and(|s| s.junk.is_some());
        let several = self.engine.accounts().len() > 1;
        let name = acct.account.display_name().to_string();
        let title = match (watch.path == "INBOX", several) {
            (true, false) => None,
            (true, true) => Some(name),
            (false, false) => Some(shown_path(&watch.path)),
            (false, true) => Some(format!("{name} › {}", shown_path(&watch.path))),
        };
        for u in fresh {
            let Some(info) = folder.message_info(&u) else { continue };
            let from = address_list(&info.from().unwrap_or_default()).into_iter().next().unwrap_or_default();
            let subject = info.subject().map(|s| s.to_string()).unwrap_or_default();
            let preview = info.preview().map(|p| salutations_core::render::clean_preview(&p)).unwrap_or_default();
            let verified =
                info.user_header("Authentication-Results").and_then(|h| salutations_core::verify::parse(&h).verified_domain(&from.email));
            watch.noted.borrow_mut().push(u.clone());
            // The contact's photo can take a moment to fetch.
            let (this, account, title, path) = (self.clone(), account.clone(), title.clone(), watch.path.clone());
            glib::spawn_future_local(async move {
                let (contact_name, photo) = this.people.lookup(&from.email, verified.as_deref()).await;
                this.notifier.message(MessageNote {
                    account: &account,
                    folder: &path,
                    title,
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

    /// The one folder all of the combined notification's messages are in.
    fn combined_folder(&self, account: &str) -> Option<String> {
        let combined = self.extra(account).combined.borrow().clone();
        let first = combined.first()?.0.clone();
        combined.iter().all(|(p, _)| *p == first).then_some(first)
    }

    fn show_combined(&self, acct: &AccountState, sound: bool) {
        let extra = self.extra(acct.uid());
        let combined = extra.combined.borrow().clone();
        let senders: Vec<String> = combined
            .iter()
            .rev()
            .filter_map(|(p, u)| self.watch(acct.uid(), p)?.folder.message_info(u))
            .filter_map(|i| address_list(&i.from().unwrap_or_default()).into_iter().next())
            .map(|a| a.display().to_string())
            .collect();
        let folder = self.combined_folder(acct.uid()).filter(|f| f != "INBOX").map(|f| shown_path(&f));
        self.notifier.combined(acct.uid(), &acct.account.display_name(), folder.as_deref(), combined.len(), &senders, sound);
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
        let (kind, account, folder, uid) = match action {
            Action::Open { account, folder, uid } => ("open", account, folder, uid),
            Action::Reply { account, folder, uid } => ("reply", account, folder, uid),
            Action::Delete { account, folder, uid } => ("delete", account, folder, uid),
            Action::Junk { account, folder, uid } => ("junk", account, folder, uid),
            Action::OpenNew { account } => {
                let folder = self.combined_folder(&account).unwrap_or_else(|| "INBOX".to_string());
                ("open-folder", account, folder, String::new())
            }
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
            self.emit("notification", (kind.to_string(), account, folder, uid).to_variant());
            return;
        }
        let settings = Settings::get();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Ok(src) = this.folder(acct.uid(), &folder).await else { return };
            let uids = vec![uid];
            let dest = if kind == "junk" {
                mark_junk(&src, &uids, true);
                if settings.read_on_junk {
                    set_flags(&src, &uids, SEEN, SEEN);
                }
                special.junk
            } else {
                if settings.read_on_delete {
                    set_flags(&src, &uids, SEEN, SEEN);
                }
                special.trash
            };
            if let Some(dest) = dest {
                this.move_now(&acct, &src, &uids, &dest).await;
            }
        });
    }

    /// Notes that messages Salutations moves or copies to `dest` are not
    /// new mail there, when `dest` is watched.
    pub(crate) fn expect(&self, account: &str, dest: &str, keys: impl IntoIterator<Item = crate::daemon::MessageKey>) {
        if self.wanted_folders(account).iter().any(|w| w.eq_ignore_ascii_case(dest)) {
            self.extra(account).expected.borrow_mut().extend(keys.into_iter().map(|k| (dest.to_string(), k)));
        }
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
        self.expect(acct.uid(), dest, uids.iter().filter_map(|u| message_key(folder, u)));
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

    /// What the rules look at in one message.
    async fn facts(&self, folder: &camel::Folder, uid: &str, headers: bool) -> Looked {
        const ATTACHMENTS: u32 = camel::MessageFlags::ATTACHMENTS.bits();
        let Some(info) = folder.message_info(uid) else { return Looked::Gone };
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
                Ok(None) => return Looked::Gone,
                Err(e) => {
                    log(&format!("rules: headers of {uid} not read: {e}"));
                    return Looked::NoHeaders(facts);
                }
            }
        }
        Looked::Ready(facts)
    }

    /// Runs `rules` on `uids` in `folder`: flags at once, then moves
    /// without undo. With `wait`, a message whose headers
    /// could not be read is left alone and returned to be tried again;
    /// otherwise its rules run without them.
    pub(crate) async fn run_rules(
        self: &Rc<Self>,
        acct: &AccountState,
        folder: &camel::Folder,
        uids: Vec<String>,
        rules: &[Rule],
        wait: bool,
    ) -> RulesRun {
        let headers = rules.iter().any(Rule::needs_headers);
        if rules.iter().any(Rule::needs_contacts) {
            let start = Instant::now();
            while self.people.known("").is_none() && start.elapsed() < CONTACTS_WAIT {
                glib::timeout_future(Duration::from_millis(250)).await;
            }
        }
        let mut kept = Vec::new();
        let mut waiting = Vec::new();
        let mut filed: Vec<(Filing, Vec<String>)> = Vec::new();
        let mut matched = 0;
        for uid in uids {
            let facts = match self.facts(folder, &uid, headers).await {
                Looked::Gone => continue,
                Looked::NoHeaders(_) if wait => {
                    waiting.push(uid);
                    continue;
                }
                Looked::NoHeaders(facts) | Looked::Ready(facts) => facts,
            };
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
        RulesRun { kept, matched, waiting }
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

    /// Runs `rule` once on every message in the folders it runs on; how
    /// many it matched.
    pub(crate) async fn apply_rule_now(self: &Rc<Self>, acct: &Rc<AccountState>, rule: Rule) -> Result<usize, glib::Error> {
        if acct.inbox().is_none() {
            return Err(glib::Error::new(gio::IOErrorEnum::NotFound, "Inbox isn't available yet; the rule will run on new mail"));
        }
        let mut matched = 0;
        for path in rule.folders() {
            let folder = self.folder(acct.uid(), &path).await?;
            let _ = folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await;
            let uids: Vec<String> =
                folder.uids().iter().map(|u| u.to_string()).filter(|u| folder.message_flags(u) & DELETED == 0).collect();
            matched += self.run_rules(acct, &folder, uids, std::slice::from_ref(&rule), false).await.matched;
        }
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
