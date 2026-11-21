//! Messages that could not be sent wait in Drafts, marked on this device,
//! and are sent again after each successful sync of their account.

use std::rc::Rc;

use glib::prelude::*;
use salutations_core::camel::{self, prelude::*};
use salutations_core::engine::AccountState;
use salutations_core::outbox::{self, Waiting};
use salutations_core::{Settings, crypto, send};

use crate::daemon::{DELETED, Daemon, Handoff, SEEN};

impl Daemon {
    /// Sends the account's waiting messages, one at a time.
    pub(crate) fn retry_waiting(self: &Rc<Self>, acct: &Rc<AccountState>) {
        let waiting: Vec<Waiting> = outbox::load().into_iter().filter(|w| w.account == acct.uid()).collect();
        let extra = self.extra(acct.uid());
        if waiting.is_empty() || extra.sending.replace(true) {
            return;
        }
        let (this, acct) = (self.clone(), acct.clone());
        glib::spawn_future_local(async move {
            for w in waiting {
                this.send_waiting(&acct, &w).await;
            }
            this.extra(acct.uid()).sending.set(false);
            this.emit("outbox", (acct.uid().to_string(), String::new()).to_variant());
        });
    }

    fn set_sending(&self, acct: &AccountState, uid: Option<&str>) {
        *self.extra(acct.uid()).sending_uid.borrow_mut() = uid.map(str::to_string);
        self.emit("outbox", (acct.uid().to_string(), uid.unwrap_or_default().to_string()).to_variant());
    }

    async fn send_waiting(self: &Rc<Self>, acct: &Rc<AccountState>, w: &Waiting) {
        let Some(drafts) = acct.special().and_then(|s| s.drafts) else { return };
        let Ok(folder) = self.folder(acct.uid(), &drafts).await else { return };
        self.set_sending(acct, Some(&w.uid));
        let msg = match folder.message_future(&w.uid, glib::ffi::G_PRIORITY_DEFAULT).await {
            Ok(Some(m)) => m,
            // Deleted or sent from elsewhere: no longer waiting.
            Ok(None) => {
                forget(&w.message_id);
                self.set_sending(acct, None);
                return;
            }
            Err(e) => {
                if e.matches(camel::FolderError::InvalidUid) {
                    forget(&w.message_id);
                }
                self.set_sending(acct, None);
                return;
            }
        };
        // Rebuilt from the draft, so it is signed and encrypted as it was
        // composed rather than as saved (drafts are never signed).
        let body = self.decrypt(&msg).await;
        let out = salutations_core::compose::from_draft_with(&msg, body.as_ref());
        let prefs = Settings::get().compose_for(acct.uid());
        let keys = prefs.signing_key.map(|signer| send::Keys { signer, autocrypt: prefs.autocrypt });
        let result = send::send(&self.engine.session, &acct.account, &out, keys).await;
        self.set_sending(acct, None);
        match result {
            Ok(sent) => {
                forget(&w.message_id);
                self.file_sent(acct, &sent).await;
                self.delete_draft(acct, &folder, &w.uid).await;
                self.emit("toast", "Message sent".to_variant());
            }
            Err(e) => {
                let mut list = outbox::load();
                if let Some(x) = list.iter_mut().find(|x| x.message_id == w.message_id) {
                    x.error = send::describe_failure(&e);
                }
                outbox::save(&list);
            }
        }
    }

    async fn decrypt(&self, msg: &camel::MimeMessage) -> Option<camel::MimePart> {
        let job = Handoff((self.gpg.clone(), msg.clone()));
        gio::spawn_blocking(move || {
            let Handoff((ctx, msg)) = job;
            Handoff(crypto::unprotect_sync(&ctx, &msg).filter(|(_, p)| p.encrypted && p.decrypt_error.is_none()).map(|(b, _)| b))
        })
        .await
        .ok()
        .and_then(|h| h.0)
    }

    async fn file_sent(self: &Rc<Self>, acct: &AccountState, msg: &camel::MimeMessage) {
        let Some(sent) = acct.special().and_then(|s| s.sent) else { return };
        let result = match self.folder(acct.uid(), &sent).await {
            Ok(folder) => send::append(&folder, msg, SEEN).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            self.emit("toast", format!("Sent, but not saved to Sent: {}", e.message()).to_variant());
        }
    }

    /// Deletes a sent draft for good: moved to Trash and expunged there,
    /// since IMAPX moves deleted messages to Trash instead of expunging.
    async fn delete_draft(self: &Rc<Self>, acct: &AccountState, folder: &camel::Folder, uid: &str) {
        let trash = acct.special().and_then(|s| s.trash).filter(|t| folder.full_name().as_deref() != Some(t.as_str()));
        let (target, uids) = match trash {
            Some(trash) => {
                let Ok(to) = self.folder(acct.uid(), &trash).await else { return };
                let job = Handoff((folder.clone(), to.clone(), vec![uid.to_string()]));
                let moved = gio::spawn_blocking(move || {
                    let Handoff((src, to, list)) = job;
                    src.transfer_messages_to_sync(&list, &to, true, None::<&gio::Cancellable>)
                })
                .await;
                match moved {
                    Ok(Ok(new)) if new.iter().all(|u| u.is_some()) => (to, new.into_iter().flatten().collect::<Vec<_>>()),
                    _ => return,
                }
            }
            None => (folder.clone(), vec![uid.to_string()]),
        };
        target.freeze();
        for u in &uids {
            target.set_message_flags(u, DELETED | SEEN, DELETED | SEEN);
        }
        target.thaw();
        let _ = target.synchronize_future(true, glib::ffi::G_PRIORITY_DEFAULT).await;
    }
}

fn forget(message_id: &str) {
    let mut list = outbox::load();
    list.retain(|w| w.message_id != message_id);
    outbox::save(&list);
}
