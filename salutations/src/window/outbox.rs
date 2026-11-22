//! Messages that could not be sent: they wait in Drafts, marked on this
//! device, and are sent again after each successful sync of their account
//! (which follows the network coming back and waking from sleep).

use salutations_core::compose::Outgoing;
use salutations_core::outbox::{self, Waiting};

use super::*;

impl Window {
    /// A send failed for a reason other than the message itself: it is
    /// saved to Drafts, marked waiting, and the composer closes.
    pub(super) fn queue_failed(self: &Rc<Self>, account: Rc<AccountCtl>, out: Outgoing, error: String) {
        let old = self.compose.borrow().as_ref().and_then(|c| c.draft_uid.clone());
        let this = self.clone();
        glib::spawn_future_local(async move {
            let uid = match this.store_draft(&account, old, &out).await {
                Ok(Some(uid)) => uid,
                Ok(None) => {
                    this.composer.set_status(Some(&format!("Not sent: {error}. Saved to Drafts.")));
                    return;
                }
                Err(e) => {
                    this.composer.set_status(Some(&format!("Not sent: {error}. Not saved: {e}")));
                    return;
                }
            };
            let mut list = outbox::load();
            list.retain(|w| w.message_id != out.message_id);
            list.push(Waiting { account: account.account.uid.clone(), uid, message_id: out.message_id.clone(), error });
            outbox::save(&list);
            // Saved and marked; the composer has nothing left to keep.
            this.compose.borrow_mut().take();
            this.composer.mark_saved();
            this.show_viewer();
            if this.inner.is_collapsed() {
                this.inner.set_show_content(false);
            }
            this.toast("Not sent. It is in Drafts and will be sent when the server can be reached.", false);
            this.update_outbox_notes();
        });
    }

    /// Marks waiting drafts in the open Drafts folder's list.
    pub(super) fn update_outbox_notes(&self) {
        let mut notes = std::collections::HashMap::new();
        if let Some(folder) = self.list.folder()
            && self.is_drafts(&folder)
            && let Some(ctl) = self.account_of(&folder)
        {
            let sending = ctl.sending_uid.borrow().clone();
            for w in outbox::load().into_iter().filter(|w| w.account == ctl.account.uid) {
                let note = if sending.as_ref() == Some(&w.uid) { "Sending…" } else { "Not sent, will retry" };
                notes.insert(w.uid, note.to_string());
            }
        }
        self.list.set_notes(notes);
        self.update_unsent();
    }

    /// Badges each account's sync button with its waiting messages.
    pub(super) fn update_unsent(&self) {
        let waiting = outbox::load();
        for ctl in self.accounts.borrow().iter() {
            let n = waiting.iter().filter(|w| w.account == ctl.account.uid).count() as u32;
            if ctl.obj.unsent() != n {
                ctl.obj.set_unsent(n);
            }
        }
    }

    /// Lists the account's waiting messages with why they were not sent.
    pub(super) fn show_unsent(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        let (this, ctl) = (self.clone(), ctl.clone());
        glib::spawn_future_local(async move {
            let waiting: Vec<Waiting> = outbox::load().into_iter().filter(|w| w.account == ctl.account.uid).collect();
            if waiting.is_empty() {
                this.update_unsent();
                return;
            }
            let drafts = ctl.eng.special().as_ref().and_then(|s| s.drafts.clone());
            let folder = match (drafts, ctl.eng.store()) {
                (Some(d), Some(store)) => this.session.folder_future(&store, &d).await.ok(),
                _ => None,
            };
            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            list.set_selection_mode(gtk::SelectionMode::None);
            let dialog = adw::AlertDialog::builder()
                .heading(if waiting.len() == 1 { "Message not sent" } else { "Messages not sent" })
                .body(format!(
                    "{} in Drafts and will be tried again after each sync of {}.",
                    if waiting.len() == 1 { "It is" } else { "They are" },
                    ctl.account.display_name()
                ))
                .extra_child(&list)
                .build();
            dialog.add_responses(&[("close", "_Close"), ("retry", "_Retry now")]);
            dialog.set_response_appearance("retry", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("retry"));
            dialog.set_close_response("close");
            for w in waiting {
                let subject = folder
                    .as_ref()
                    .and_then(|f| f.message_info(&w.uid))
                    .and_then(|i| i.subject().map(|s| s.to_string()))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "(No subject)".to_string());
                let row = adw::ActionRow::builder().title(subject).subtitle(&w.error).use_markup(false).build();
                let edit = gtk::Button::builder().label("_Edit").use_underline(true).valign(gtk::Align::Center).build();
                edit.add_css_class("flat");
                let delete = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Delete")
                    .valign(gtk::Align::Center)
                    .build();
                delete.add_css_class("flat");
                row.add_suffix(&edit);
                row.add_suffix(&delete);
                list.append(&row);
                if let Some(f) = folder.clone() {
                    let (this, d, uid) = (this.clone(), dialog.clone(), w.uid.clone());
                    edit.connect_clicked(move |_| {
                        d.close();
                        this.open_draft(f.clone(), uid.clone());
                    });
                } else {
                    edit.set_sensitive(false);
                }
                let (this, ctl, d, list, row) = (this.clone(), ctl.clone(), dialog.clone(), list.clone(), row.clone());
                delete.connect_clicked(move |_| {
                    let (this, ctl, d, list, row, w) = (this.clone(), ctl.clone(), d.clone(), list.clone(), row.clone(), w.clone());
                    glib::spawn_future_local(async move {
                        let confirm = adw::AlertDialog::builder()
                            .heading("Delete this message?")
                            .body("It will be deleted from Drafts permanently. This cannot be undone.")
                            .build();
                        confirm.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
                        confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                        confirm.set_default_response(Some("cancel"));
                        confirm.set_close_response("cancel");
                        if confirm.choose_future(Some(&this.window)).await != "delete" {
                            return;
                        }
                        forget(&w.message_id);
                        this.update_outbox_notes();
                        list.remove(&row);
                        if list.first_child().is_none() {
                            d.close();
                        }
                        this.delete_draft(&ctl, w.uid.clone()).await;
                    });
                });
            }
            if dialog.choose_future(Some(&this.window)).await == "retry" {
                this.engine.sync(&ctl.eng);
            }
        });
    }

    /// The waiting message with this UID in the account's Drafts, if any.
    pub(super) fn waiting(&self, ctl: &AccountCtl, folder: &camel::Folder, uid: &str) -> Option<Waiting> {
        if !self.is_drafts(folder) {
            return None;
        }
        outbox::load().into_iter().find(|w| w.account == ctl.account.uid && w.uid == uid)
    }
}

/// Drops a message from the waiting list.
pub(super) fn forget(message_id: &str) {
    let mut list = outbox::load();
    list.retain(|w| w.message_id != message_id);
    outbox::save(&list);
}
