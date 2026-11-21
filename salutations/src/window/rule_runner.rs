//! Rules from the window: running one once on everything in INBOX when
//! the user asks (the background process runs them), and making a rule
//! from the shown message.

use salutations_core::rules::{Condition, Field, Rule, TextOp};

use super::*;
use crate::rule_editor::{self, Edit};

/// The addresses in a summary field such as "Ann <ann@x.org>, bob@y.org".
fn address_list(raw: &str) -> Vec<salutations_core::Address> {
    let list = camel::InternetAddress::new();
    let n = list.unformat(raw);
    (0..n.max(0))
        .filter_map(|i| list.get(i))
        .filter_map(|(name, email)| {
            Some(salutations_core::Address { name: name.map(|n| n.to_string()).filter(|n| !n.is_empty()), email: email?.to_string() })
        })
        .collect()
}

impl Window {
    /// Runs `rule` once on every message in the account's INBOX, in the
    /// background process.
    pub(super) async fn apply_rule_now(self: &Rc<Self>, account: &str, rule: Rule) {
        let args = (account.to_string(), salutations_core::rules::to_text(&rule)).to_variant();
        let text = match remote::call("apply_rule", args).await {
            Ok(v) => match v.get::<u32>().unwrap_or(0) {
                0 => "The rule matched no messages in Inbox".to_string(),
                1 => "Rule applied to 1 message".to_string(),
                n => format!("Rule applied to {n} messages"),
            },
            Err(e) => e.message().to_string(),
        };
        self.toast(&text, false);
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
