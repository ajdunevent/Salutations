//! Messages that could not be sent: they wait in Drafts, marked on this
//! device, and are sent again after each successful sync of their account
//! (which follows the network coming back and waking from sleep).

use salutations_core::compose::Outgoing;
use salutations_core::outbox::{self, Waiting};
use salutations_core::send;

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

    /// Sends the account's waiting messages, one at a time.
    pub(super) fn retry_waiting(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        let waiting: Vec<Waiting> = outbox::load().into_iter().filter(|w| w.account == ctl.account.uid).collect();
        if waiting.is_empty() || ctl.sending.replace(true) {
            return;
        }
        let (this, ctl) = (self.clone(), ctl.clone());
        glib::spawn_future_local(async move {
            for w in waiting {
                this.send_waiting(&ctl, &w).await;
            }
            ctl.sending.set(false);
            this.update_outbox_notes();
        });
    }

    async fn send_waiting(self: &Rc<Self>, ctl: &Rc<AccountCtl>, w: &Waiting) {
        let drafts = ctl.special.borrow().as_ref().and_then(|s| s.drafts.clone());
        let store = ctl.store.borrow().clone();
        let (Some(drafts), Some(store)) = (drafts, store) else { return };
        let Ok(folder) = self.session.folder_future(&store, &drafts).await else { return };
        *ctl.sending_uid.borrow_mut() = Some(w.uid.clone());
        self.update_outbox_notes();
        let msg = match folder.message_future(&w.uid, glib::ffi::G_PRIORITY_DEFAULT).await {
            Ok(Some(m)) => m,
            // Deleted or sent from elsewhere: no longer waiting.
            Ok(None) => {
                forget(&w.message_id);
                ctl.sending_uid.borrow_mut().take();
                return;
            }
            Err(e) => {
                if e.matches(camel::FolderError::InvalidUid) {
                    forget(&w.message_id);
                }
                ctl.sending_uid.borrow_mut().take();
                return;
            }
        };
        // Rebuilt from the draft, so it is signed and encrypted as it was
        // composed rather than as saved (drafts are never signed).
        let body = self.decrypt(&msg).await;
        let out = salutations_core::compose::from_draft_with(&msg, body.as_ref());
        let keys = self.account_keys(ctl);
        let result = send::send(&self.session, &ctl.account, &out, keys).await;
        ctl.sending_uid.borrow_mut().take();
        match result {
            Ok(sent) => {
                forget(&w.message_id);
                self.file_sent(ctl, &sent).await;
                let t = Target { account: ctl.clone(), folder, uids: vec![w.uid.clone()], thread: false };
                self.delete_permanently(&t).await;
                self.toast("Message sent", false);
            }
            Err(e) => {
                let mut list = outbox::load();
                if let Some(x) = list.iter_mut().find(|x| x.message_id == w.message_id) {
                    x.error = salutations_core::send::describe_failure(&e);
                }
                outbox::save(&list);
            }
        }
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
