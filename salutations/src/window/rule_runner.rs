//! Running an account's rules: on new INBOX mail before it is notified
//! about, once on everything in INBOX when the user asks, and making a
//! rule from the shown message.

use std::time::{Duration, Instant};

use salutations_core::rules::{Condition, Dmarc, Facts, Field, Filing, Rule, TextOp};

use super::*;
use crate::rule_editor::{self, Edit};

/// How long rules that ask about contacts wait for the address books at
/// startup before taking every sender as unknown.
const CONTACTS_WAIT: Duration = Duration::from_secs(10);

/// The addresses in a summary field such as "Ann <ann@x.org>, bob@y.org".
fn address_list(raw: &str) -> Vec<salutations_core::Address> {
    let list = camel::InternetAddress::new();
    let n = list.unformat(raw);
    (0..n.max(0))
        .filter_map(|i| list.get(i))
        .filter_map(|(name, email)| {
            Some(salutations_core::Address {
                name: name.map(|n| n.to_string()).filter(|n| !n.is_empty()),
                email: email?.to_string(),
            })
        })
        .collect()
}

/// What the rules look at in one message; None when it is gone.
async fn facts(folder: &camel::Folder, uid: &str, headers: bool) -> Option<Facts> {
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
        in_contacts: crate::people::known(&email).unwrap_or(false),
        from,
        to_cc,
        headers: Vec::new(),
    };
    if headers {
        match folder.message_future(uid, glib::ffi::G_PRIORITY_DEFAULT).await {
            Ok(Some(msg)) => facts.headers = salutations_core::render::headers(&msg),
            Ok(None) => {}
            Err(e) => crate::debug::log(&format!("rules: headers of {uid} not read: {e}")),
        }
    }
    Some(facts)
}

impl Window {
    /// Runs `rules` on `uids` in `folder`, the account's INBOX: flags at
    /// once, moves through the queue without undo. Returns the messages
    /// left in INBOX and how many matched a rule.
    pub(super) async fn run_rules(
        self: &Rc<Self>,
        ctl: &Rc<AccountCtl>,
        folder: &camel::Folder,
        uids: Vec<String>,
        rules: &[Rule],
    ) -> (Vec<String>, usize) {
        let headers = rules.iter().any(Rule::needs_headers);
        if rules.iter().any(Rule::needs_contacts) {
            let start = Instant::now();
            while crate::people::known("").is_none() && start.elapsed() < CONTACTS_WAIT {
                glib::timeout_future(Duration::from_millis(250)).await;
            }
        }
        let mut kept = Vec::new();
        let mut filed: Vec<(Filing, Vec<String>)> = Vec::new();
        let mut matched = 0;
        for uid in uids {
            let Some(facts) = facts(folder, &uid, headers).await else { continue };
            let out = salutations_core::rules::apply(rules, &facts);
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
            if !self.file_by_rule(ctl, folder, &filing, &uids) {
                kept.extend(uids);
            }
        }
        if matched > 0 {
            crate::debug::log(&format!("rules: {matched} messages matched in {}", ctl.account.uid));
            self.update_viewer_read();
        }
        (kept, matched)
    }

    /// Queues the move a rule asks for; false when there is nowhere to go
    /// (no Archive or Junk folder, a folder since deleted).
    fn file_by_rule(self: &Rc<Self>, ctl: &Rc<AccountCtl>, folder: &camel::Folder, filing: &Filing, uids: &[String]) -> bool {
        let special = ctl.special.borrow().clone().unwrap_or_default();
        let dest = match filing {
            Filing::Folder(f) => ctl.obj.find(f).is_some().then(|| f.clone()),
            Filing::Archive => special.archive,
            Filing::Junk => special.junk,
            Filing::Trash => special.trash,
        };
        let here = folder.full_name().map(|n| n.to_string());
        let Some(dest) = dest.filter(|d| Some(d) != here.as_ref()) else {
            crate::debug::log(&format!("rules: nowhere to file {filing:?}"));
            return false;
        };
        let t = Target { account: ctl.clone(), folder: folder.clone(), uids: uids.to_vec(), thread: false };
        let settings = Settings::get();
        match filing {
            Filing::Junk => {
                mark_junk(&t, true);
                if settings.read_on_junk {
                    self.mark_read_moving(&t);
                }
            }
            Filing::Trash if settings.read_on_delete => self.mark_read_moving(&t),
            _ => {}
        }
        self.enqueue(&t, &dest, false);
        true
    }

    /// Runs `rule` once on every message in the account's INBOX.
    pub(super) async fn apply_rule_now(self: &Rc<Self>, account: &str, rule: Rule) {
        let ctl = self.accounts.borrow().iter().find(|c| c.account.uid == account).cloned();
        let Some(ctl) = ctl else { return };
        let inbox = ctl.inbox.borrow().as_ref().map(|(f, _)| f.clone());
        let Some(folder) = inbox else {
            self.toast("Inbox isn't available yet; the rule will run on new mail", false);
            return;
        };
        let uids: Vec<String> = folder
            .uids()
            .iter()
            .map(|u| u.to_string())
            .filter(|u| folder.message_flags(u) & DELETED == 0)
            .collect();
        let (_, matched) = self.run_rules(&ctl, &folder, uids, std::slice::from_ref(&rule)).await;
        self.toast(
            &match matched {
                0 => "The rule matched no messages in Inbox".to_string(),
                1 => "Rule applied to 1 message".to_string(),
                n => format!("Rule applied to {n} messages"),
            },
            false,
        );
    }

    /// A new rule for the shown message: for its mailing list when it has
    /// a List-Id, otherwise for its sender.
    pub(super) async fn create_rule(self: &Rc<Self>) {
        let Some((folder, uid)) = self.viewer.target() else { return };
        let Some(ctl) = self.account_of(&folder) else { return };
        let from = folder.message_info(&uid).map(|i| address_list(&i.from().unwrap_or_default())).unwrap_or_default();
        let list = self.viewer.message().and_then(|m| m.header("List-Id")).map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
        let text = |field, op, value: String| Condition::Text { field, header: String::new(), op, value };
        let rule = match (list, from.first()) {
            (Some(list), _) => {
                // "Library news <news.example.org>": the id in brackets
                // stays the same when the list's name changes.
                let id = list.rsplit_once('<').and_then(|(_, r)| r.split_once('>')).map(|(id, _)| id.trim().to_string());
                let name = list.split('<').next().map(|n| n.trim().trim_matches('"').to_string()).filter(|n| !n.is_empty());
                Rule {
                    name: name.clone().or_else(|| id.clone()).unwrap_or_else(|| list.clone()),
                    conditions: vec![text(Field::ListId, TextOp::Contains, id.unwrap_or(list))],
                    ..Default::default()
                }
            }
            (None, Some(sender)) => Rule {
                name: sender.display().to_string(),
                conditions: vec![text(Field::From, TextOp::Is, sender.email.clone())],
                ..Default::default()
            },
            (None, None) => Rule::default(),
        };
        let Edit::Save(rule) = rule_editor::edit(&self.window, &ctl.account.uid, &folder_paths(&ctl), rule, false).await else { return };
        let account = ctl.account.uid.clone();
        rule_editor::append(&account, rule.clone());
        let weak = Rc::downgrade(self);
        let apply: rule_editor::ApplyRule = Rc::new(move |uid: &str, rule| {
            let (Some(this), uid) = (weak.upgrade(), uid.to_string()) else { return };
            glib::spawn_future_local(async move { this.apply_rule_now(&uid, rule).await });
        });
        rule_editor::offer_apply(&self.window, &account, rule, &apply).await;
    }
}
