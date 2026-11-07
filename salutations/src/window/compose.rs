//! Composing in the window: opening the composer for a new message, a
//! reply, a forward or a draft, saving drafts and sending.
//!
//! Drafts are saved to the account's Drafts folder 30 s after the last
//! edit, at least every 2 minutes while edits continue, and when the
//! composer closes. Each save appends a new copy and deletes the previous
//! one, so saves are spaced out rather than made on every pause.

use std::time::{Duration, Instant};

use salutations_core::compose::{self, Format, Original, Outgoing};
use salutations_core::send;

use super::*;
use crate::composer::{CryptoSetup, FromChoice, Kind, Protection};
use salutations_core::crypto;

const SAVE_IDLE: Duration = Duration::from_secs(30);
const SAVE_MAX: Duration = Duration::from_secs(120);

/// The open composer's message and where it came from.
pub(super) struct Compose {
    account: Rc<AccountCtl>,
    /// The saved copy in Drafts, replaced by each save.
    pub(super) draft_uid: Option<String>,
    /// The message replied to or forwarded: shown again when the composer
    /// closes, and flagged Answered once a reply is sent.
    original: Option<(camel::Folder, String)>,
    kind: Kind,
    idle: Option<glib::SourceId>,
    /// The first edit not yet saved.
    unsaved_since: Option<Instant>,
}

impl Drop for Compose {
    fn drop(&mut self) {
        if let Some(id) = self.idle.take() {
            id.remove();
        }
    }
}

/// A fingerprint in groups of four, as GnuPG prints it.
pub(super) fn group_fingerprint(fpr: &str) -> String {
    fpr.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(" ")
}

pub(super) fn date_text(date: Option<i64>) -> String {
    date.and_then(|d| glib::DateTime::from_unix_local(d).ok())
        .and_then(|d| d.format(&format!("%a, %b %e, %Y, {}", crate::clock::time_format())).ok())
        // The clock format pads hours with a figure space for alignment.
        .map(|s| s.replace('\u{2007}', "").split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// Flags a reply's original Answered, a forward's `$Forwarded`, and with
/// "Mark messages as read" set to on reply or forward, either one read.
fn mark_responded((folder, uid): &(camel::Folder, String), kind: Kind) {
    const ANSWERED: u32 = camel::MessageFlags::ANSWERED.bits();
    match kind {
        Kind::Reply => folder.set_message_flags(uid, ANSWERED, ANSWERED),
        Kind::Forward => {
            if let Some(info) = folder.message_info(uid) {
                camel::message_info_set_user_flag(&info, FORWARDED, true);
            }
            true
        }
        _ => return,
    };
    let settings = Settings::get();
    if settings.mark_read && settings.read_on_response {
        folder.set_message_flags(uid, SEEN, SEEN);
    }
}

use crate::message_list::FORWARDED;

impl Window {
    pub(super) fn connect_composer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.composer.connect_send(move || {
            if let Some(this) = weak.upgrade() {
                this.send_composed();
            }
        });
        let weak = Rc::downgrade(self);
        self.composer.connect_discard(move || {
            if let Some(this) = weak.upgrade() {
                this.discard_composed();
            }
        });
        let weak = Rc::downgrade(self);
        self.composer.connect_account_changed(move |uid| {
            if let Some(this) = weak.upgrade() {
                this.composer_account_changed(&uid);
            }
        });
        let weak = Rc::downgrade(self);
        self.composer.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.schedule_save();
            }
        });
        let actions: [(&str, Box<dyn Fn(&Rc<Window>)>); 8] = [
            ("compose", Box::new(|w| w.compose_new())),
            ("reply", Box::new(|w| w.compose_reply(false, None))),
            ("reply-all", Box::new(|w| w.compose_reply(true, None))),
            ("reply-plain", Box::new(|w| w.compose_reply(false, Some(Format::Plain)))),
            ("reply-markdown", Box::new(|w| w.compose_reply(false, Some(Format::Markdown)))),
            ("reply-all-plain", Box::new(|w| w.compose_reply(true, Some(Format::Plain)))),
            ("reply-all-markdown", Box::new(|w| w.compose_reply(true, Some(Format::Markdown)))),
            ("forward", Box::new(|w| w.compose_forward(false))),
        ];
        for (name, f) in actions.into_iter().chain([(
            "forward-attachment",
            Box::new(|w: &Rc<Window>| w.compose_forward(true)) as Box<dyn Fn(&Rc<Window>)>,
        )]) {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            self.window.add_action(&a);
        }
    }

    /// The account a new message is from: the open folder's, else the
    /// first.
    fn compose_account(&self) -> Option<Rc<AccountCtl>> {
        let open = self.open.borrow().as_ref().map(|o| o.account.clone());
        open.or_else(|| self.accounts.borrow().first().cloned())
    }

    fn compose_new(self: &Rc<Self>) {
        let Some(account) = self.compose_account() else { return };
        let prefs = Settings::get().compose_for(&account.account.uid);
        let out = Outgoing {
            from: account.account.sender(),
            format: prefs.format,
            message_id: compose::new_message_id(&account.account.sender().email),
            sign: prefs.signing_key.is_some(),
            ..Default::default()
        };
        self.start_compose(account, Kind::New, out, None, None, true);
    }

    /// The shown message with its account, folder and UID.
    fn shown_original(&self) -> Option<(Rc<AccountCtl>, camel::Folder, String, camel::MimeMessage)> {
        let (folder, uid) = self.viewer.current()?;
        let msg = self.viewer.message()?;
        let account = self.account_of(&folder)?;
        Some((account, folder, uid, msg))
    }

    fn compose_reply(self: &Rc<Self>, all: bool, format: Option<Format>) {
        let Some((account, folder, uid, msg)) = self.shown_original() else { return };
        let orig = Original::from_parts(&msg, self.viewer.body().as_ref());
        let prefs = Settings::get().compose_for(&account.account.uid);
        let encrypted = self.viewer.protection().is_some_and(|p| p.encrypted);
        let (to, cc) = compose::reply_recipients(&orig, all, &account.account.own_addresses());
        let (in_reply_to, references) = compose::threading(&orig);
        let out = Outgoing {
            from: self.reply_from(&account, &orig),
            to: compose::format_addresses(&to),
            cc: compose::format_addresses(&cc),
            subject: compose::reply_subject(&orig.subject),
            body: format!("\n\n{}", compose::reply_quote(&orig, &date_text(orig.date))),
            format: format.unwrap_or(prefs.format),
            message_id: compose::new_message_id(&account.account.sender().email),
            in_reply_to,
            references,
            sign: prefs.signing_key.is_some(),
            encrypt: prefs.signing_key.is_some() && ((encrypted && prefs.encrypt_replies) || prefs.encrypt_new),
            ..Default::default()
        };
        self.start_compose(account, Kind::Reply, out, Some((folder, uid)), None, true);
    }

    /// Replies go out from the alias the original was sent to, when it was
    /// sent to one.
    fn reply_from(&self, account: &AccountCtl, orig: &Original) -> salutations_core::Address {
        let recipients: Vec<&salutations_core::Address> = orig.to.iter().chain(&orig.cc).collect();
        account
            .account
            .aliases()
            .into_iter()
            .find(|a| recipients.iter().any(|r| r.email.eq_ignore_ascii_case(&a.email)))
            .unwrap_or_else(|| account.account.sender())
    }

    fn compose_forward(self: &Rc<Self>, attach: bool) {
        let Some((account, folder, uid, msg)) = self.shown_original() else { return };
        let body = self.viewer.body();
        let orig = Original::from_parts(&msg, body.as_ref());
        let prefs = Settings::get().compose_for(&account.account.uid);
        let mut out = Outgoing {
            from: self.reply_from(&account, &orig),
            subject: compose::forward_subject(&orig.subject),
            format: prefs.format,
            message_id: compose::new_message_id(&account.account.sender().email),
            sign: prefs.signing_key.is_some(),
            forwarded_id: orig.message_id.clone(),
            ..Default::default()
        };
        if attach {
            out.forwarded = Some(msg);
        } else {
            out.body = format!("\n\n{}", compose::forward_inline(&orig, &date_text(orig.date)));
            out.attachments = match &body {
                Some(b) => salutations_core::render::attachments_in(b),
                None => salutations_core::render::attachments(&msg),
            };
        }
        self.start_compose(account, Kind::Forward, out, Some((folder, uid)), None, true);
    }

    /// Opens a message from Drafts in the composer.
    pub(super) fn open_draft(self: &Rc<Self>, folder: camel::Folder, uid: String) {
        let Some(account) = self.account_of(&folder) else { return };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let msg = match folder.message_future(&uid, glib::ffi::G_PRIORITY_DEFAULT).await {
                Ok(Some(m)) => m,
                Ok(None) => return this.toast("The server returned no message", false),
                Err(e) => return this.toast(&format!("Cannot open the draft: {}", e.message()), false),
            };
            // Editing a waiting message takes it out of the queue; Send
            // queues it again if it still cannot go.
            if let Some(w) = this.waiting(&account, &folder, &uid) {
                super::outbox::forget(&w.message_id);
                this.update_outbox_notes();
            }
            // A draft saved with Encrypt on is encrypted to the sender.
            let body = this.decrypt(&msg).await;
            let mut out = compose::from_draft_with(&msg, body.as_ref());
            if out.message_id.is_empty() {
                out.message_id = compose::new_message_id(&account.account.sender().email);
            }
            // Its signature is already in the text.
            this.start_compose(account, Kind::Draft, out, None, Some(uid), false);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn start_compose(
        self: &Rc<Self>,
        account: Rc<AccountCtl>,
        kind: Kind,
        out: Outgoing,
        replied: Option<(camel::Folder, String)>,
        draft_uid: Option<String>,
        add_signature: bool,
    ) {
        self.close_composer();
        self.cancel_read_timer();
        self.list.unselect();
        self.viewer.forget();
        let prefs = Settings::get().compose_for(&account.account.uid);
        let froms = self.from_choices();
        self.composer.load(
            kind,
            &out,
            froms,
            &account.account.uid,
            &prefs.signature,
            add_signature && prefs.include_signature,
            prefs.signature_above,
            &account.account.display_name(),
        );
        // A draft keeps the choice it was saved with.
        let encrypt = out.encrypt || (matches!(kind, Kind::New | Kind::Forward) && prefs.encrypt_new);
        self.composer.set_crypto(CryptoSetup {
            available: prefs.signing_key.is_some(),
            protection: match (encrypt, out.sign) {
                (true, _) => Protection::SignEncrypt,
                (false, true) => Protection::Sign,
                (false, false) => Protection::None,
            },
            autocrypt: prefs.autocrypt,
            attach: prefs.attach_key,
        });
        self.load_composer_key(&account, &out.from.email, &out.message_id);
        *self.compose.borrow_mut() =
            Some(Compose { account, draft_uid, original: replied, kind, idle: None, unsaved_since: None });
        self.content_stack.set_visible_child_name("composer");
        // Also when not collapsed, so the composer stays in view if the
        // window narrows.
        self.inner.set_show_content(true);
        self.update_viewer_read();
        self.composer.focus();
    }

    /// Every account's address and aliases, in account order.
    fn from_choices(&self) -> Vec<FromChoice> {
        let mut out = Vec::new();
        for ctl in self.accounts.borrow().iter() {
            let mut addresses = vec![ctl.account.sender()];
            for a in ctl.account.aliases() {
                if !addresses.iter().any(|f| f.email.eq_ignore_ascii_case(&a.email)) {
                    addresses.push(a);
                }
            }
            out.extend(addresses.into_iter().map(|address| FromChoice {
                account: ctl.account.uid.clone(),
                account_name: ctl.account.display_name(),
                address,
            }));
        }
        out
    }

    /// Exports the account's public key for the composer's key tile.
    fn load_composer_key(&self, account: &AccountCtl, email: &str, message_id: &str) {
        let Some(signer) = Settings::get().compose_for(&account.account.uid).signing_key else { return };
        let (ctx, email, composer) = (self.gpg.clone(), email.to_string(), self.composer.clone());
        let (message_id, uid) = (message_id.to_string(), account.account.uid.clone());
        glib::spawn_future_local(async move {
            struct Handoff<T>(T);
            unsafe impl<T> Send for Handoff<T> {}
            let job = Handoff(ctx);
            let key = gio::spawn_blocking(move || {
                let Handoff(ctx) = job;
                Handoff(crypto::key_attachment_sync(&ctx, &signer, &email))
            })
            .await;
            // The composer may hold another message, or another account's,
            // by now.
            if let Ok(Handoff(Ok(key))) = key
                && composer.outgoing().message_id == message_id
                && composer.account() == uid
            {
                composer.set_key(key);
            }
        });
    }

    /// Another account chosen in From: the message becomes that account's,
    /// and a draft already saved moves to its Drafts.
    fn composer_account_changed(self: &Rc<Self>, uid: &str) {
        let Some(new) = self.accounts.borrow().iter().find(|c| c.account.uid == uid).cloned() else { return };
        let old = {
            let mut c = self.compose.borrow_mut();
            let Some(c) = c.as_mut() else { return };
            if Rc::ptr_eq(&c.account, &new) {
                return;
            }
            let old_account = std::mem::replace(&mut c.account, new.clone());
            c.draft_uid.take().map(|d| (old_account, d))
        };
        let prefs = Settings::get().compose_for(&new.account.uid);
        self.composer.switch_account(
            &new.account.display_name(),
            &prefs.signature,
            prefs.signature_above,
            CryptoSetup {
                available: prefs.signing_key.is_some(),
                protection: Protection::None,
                autocrypt: prefs.autocrypt,
                attach: prefs.attach_key,
            },
        );
        let out = self.composer.outgoing();
        self.load_composer_key(&new, &out.from.email, &out.message_id);
        self.composer.mark_dirty();
        let Some((old_account, old_uid)) = old else {
            self.schedule_save();
            return;
        };
        // Saved to the new account first, so the message is never only in
        // the composer.
        let this = self.clone();
        glib::spawn_future_local(async move {
            let out = this.composer.outgoing();
            this.composer.mark_saved();
            match this.store_draft(&new, None, &out).await {
                Ok(uid) => {
                    if let Some(c) = this.compose.borrow_mut().as_mut()
                        && Rc::ptr_eq(&c.account, &new)
                        && this.composer.outgoing().message_id == out.message_id
                    {
                        c.draft_uid = uid;
                    }
                    this.delete_draft(&old_account, old_uid).await;
                }
                Err(e) => {
                    this.composer.mark_dirty();
                    this.composer.set_status(Some(&format!("Draft not saved: {e}")));
                }
            }
        });
    }

    /// A new message from outside the window: a `mailto:` link, the
    /// command line or the desktop's New message action. It starts from
    /// the default account; a message being written is saved first.
    pub(crate) fn compose_external(self: &Rc<Self>, mailto: Option<salutations_core::mailto::Mailto>) {
        let account = {
            let accounts = self.accounts.borrow();
            let default = Settings::get().default_account;
            default
                .and_then(|d| accounts.iter().find(|c| c.account.uid == d).cloned())
                .or_else(|| accounts.first().cloned())
        };
        let Some(account) = account else { return };
        let prefs = Settings::get().compose_for(&account.account.uid);
        let m = mailto.unwrap_or_default();
        let out = Outgoing {
            from: account.account.sender(),
            to: m.to,
            cc: m.cc,
            bcc: m.bcc,
            subject: m.subject,
            body: m.body,
            in_reply_to: m.in_reply_to.clone(),
            references: m.in_reply_to.into_iter().collect(),
            format: prefs.format,
            message_id: compose::new_message_id(&account.account.sender().email),
            sign: prefs.signing_key.is_some(),
            ..Default::default()
        };
        self.window.present();
        self.start_compose(account, Kind::New, out, None, None, true);
    }

    fn schedule_save(self: &Rc<Self>) {
        let due = {
            let mut c = self.compose.borrow_mut();
            let Some(c) = c.as_mut() else { return };
            if let Some(id) = c.idle.take() {
                id.remove();
            }
            let since = *c.unsaved_since.get_or_insert_with(Instant::now);
            let max_left = SAVE_MAX.saturating_sub(since.elapsed());
            let weak = Rc::downgrade(self);
            c.idle = Some(glib::timeout_add_local_once(SAVE_IDLE.min(max_left), move || {
                if let Some(this) = weak.upgrade() {
                    if let Some(c) = this.compose.borrow_mut().as_mut() {
                        c.idle = None;
                    }
                    this.save_composed();
                }
            }));
            max_left.is_zero()
        };
        if due {
            self.save_composed();
        }
    }

    /// Saves the open composer's message to Drafts if it changed.
    fn save_composed(self: &Rc<Self>) {
        if !self.composer.is_dirty() {
            return;
        }
        let (account, old) = {
            let mut c = self.compose.borrow_mut();
            let Some(c) = c.as_mut() else { return };
            if let Some(id) = c.idle.take() {
                id.remove();
            }
            c.unsaved_since = None;
            (c.account.clone(), c.draft_uid.take())
        };
        let out = self.composer.outgoing();
        self.composer.mark_saved();
        let this = self.clone();
        glib::spawn_future_local(async move {
            match this.store_draft(&account, old.clone(), &out).await {
                Ok(uid) => {
                    // The composer may have closed or moved on meanwhile;
                    // the UID belongs to this message only.
                    if let Some(c) = this.compose.borrow_mut().as_mut()
                        && Rc::ptr_eq(&c.account, &account)
                        && this.composer.outgoing().message_id == out.message_id
                    {
                        c.draft_uid = uid;
                    }
                    this.composer.set_status(None);
                }
                Err(e) => {
                    if let Some(c) = this.compose.borrow_mut().as_mut()
                        && this.composer.outgoing().message_id == out.message_id
                    {
                        c.draft_uid = old;
                    }
                    this.composer.mark_dirty();
                    this.composer.set_status(Some(&format!("Draft not saved: {e}")));
                }
            }
        });
    }

    /// Appends `out` to the account's Drafts and deletes the copy `old`.
    /// Returns the new copy's UID when it can be found.
    pub(super) async fn store_draft(self: &Rc<Self>, account: &Rc<AccountCtl>, old: Option<String>, out: &Outgoing) -> Result<Option<String>, String> {
        let drafts = account.special.borrow().as_ref().and_then(|s| s.drafts.clone());
        let drafts = drafts.ok_or("this account has no Drafts folder")?;
        let store = account.store.borrow().clone().ok_or("the account is not connected")?;
        let folder = self.session.folder_future(&store, &drafts).await.map_err(|e| e.message().to_string())?;
        let keys = self.composer_keys(account);
        let msg = send::draft(&self.session, out, keys).await.map_err(|e| e.message().to_string())?;
        let before: HashSet<String> = folder.uids().into_iter().map(|u| u.to_string()).collect();
        let uid = send::append(&folder, &msg, send::DRAFT_FLAGS).await.map_err(|e| e.message().to_string())?;
        let uid = match uid {
            Some(u) => Some(u),
            None => {
                // Without UIDPLUS the new copy is the one UID that appeared.
                let _ = folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await;
                let mut new: Vec<String> =
                    folder.uids().into_iter().map(|u| u.to_string()).filter(|u| !before.contains(u)).collect();
                (new.len() == 1).then(|| new.remove(0))
            }
        };
        if let Some(old) = old.filter(|o| uid.as_ref() != Some(o)) {
            let t = Target { account: account.clone(), folder: folder.clone(), uids: vec![old], thread: false };
            self.delete_permanently(&t).await;
        }
        Ok(uid)
    }

    /// Closes the composer, saving its message to Drafts if it changed.
    pub(super) fn close_composer(self: &Rc<Self>) {
        let Some(state) = self.compose.borrow_mut().take() else { return };
        self.show_viewer();
        if !self.composer.is_dirty() {
            return;
        }
        let out = self.composer.outgoing();
        self.composer.mark_saved();
        let this = self.clone();
        let (account, old) = (state.account.clone(), state.draft_uid.clone());
        drop(state);
        glib::spawn_future_local(async move {
            if let Err(e) = this.store_draft(&account, old, &out).await {
                this.toast(&format!("Draft not saved: {e}"), false);
            }
        });
    }

    /// Saves the composer's unsaved changes, for quitting and closing the
    /// window, which end the process before a background save would finish.
    pub(crate) async fn save_before_exit(self: &Rc<Self>) {
        let Some((account, old)) = self.compose.borrow().as_ref().map(|c| (c.account.clone(), c.draft_uid.clone())) else {
            return;
        };
        if !self.composer.is_dirty() {
            return;
        }
        let out = self.composer.outgoing();
        match self.store_draft(&account, old, &out).await {
            Ok(uid) => {
                self.composer.mark_saved();
                if let Some(c) = self.compose.borrow_mut().as_mut() {
                    c.draft_uid = uid;
                }
            }
            Err(e) => glib::g_warning!("salutations", "Draft not saved: {}", e),
        }
    }

    /// Closing the window without running in background waits for the
    /// draft to be saved and for queued moves.
    pub(super) fn connect_close_saving(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let saving = Rc::new(Cell::new(false));
        self.window.connect_close_request(move |win| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            let draft = this.composer.is_dirty() && this.compose.borrow().is_some();
            if win.hides_on_close() || (!draft && this.queue.borrow().is_empty()) {
                return glib::Propagation::Proceed;
            }
            if !saving.replace(true) {
                let (win, saving) = (win.clone(), saving.clone());
                glib::spawn_future_local(async move {
                    this.save_before_exit().await;
                    this.composer.mark_saved();
                    this.finish_queue().await;
                    win.close();
                    saving.set(false);
                });
            }
            glib::Propagation::Stop
        });
    }

    fn discard_composed(self: &Rc<Self>) {
        let Some(state) = self.compose.borrow_mut().take() else { return };
        self.composer.mark_saved();
        self.return_from_composer(state.original.clone());
        let Some(uid) = state.draft_uid.clone() else { return };
        let account = state.account.clone();
        drop(state);
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.delete_draft(&account, uid).await;
        });
    }

    async fn delete_draft(self: &Rc<Self>, account: &Rc<AccountCtl>, uid: String) {
        let drafts = account.special.borrow().as_ref().and_then(|s| s.drafts.clone());
        let store = account.store.borrow().clone();
        let (Some(drafts), Some(store)) = (drafts, store) else { return };
        if let Ok(folder) = self.session.folder_future(&store, &drafts).await {
            let t = Target { account: account.clone(), folder, uids: vec![uid], thread: false };
            self.delete_permanently(&t).await;
        }
    }

    /// Send: an encrypted message first needs a key for every recipient.
    fn send_composed(self: &Rc<Self>) {
        if self.compose.borrow().is_none() {
            return;
        }
        let out = self.composer.outgoing();
        if !out.encrypt {
            self.transmit_composed();
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let emails = out.recipient_emails();
            let statuses = this.key_statuses(&emails).await;
            if statuses.iter().all(|(_, ok)| *ok) {
                this.transmit_composed();
            } else {
                this.show_key_check(statuses);
            }
        });
    }

    /// Each recipient with whether the keyring has a usable key for them.
    async fn key_statuses(&self, emails: &[String]) -> Vec<(String, bool)> {
        let (ctx, asked) = (self.gpg.clone(), emails.to_vec());
        let missing = gio::spawn_blocking(move || crypto::missing_keys_sync(&ctx, &asked)).await.unwrap_or_default();
        emails.iter().map(|e| (e.clone(), !missing.contains(e))).collect()
    }

    /// Lists the recipients and their keys, with Look Up Missing Keys and
    /// Return to Message; Send unlocks once every recipient has a key.
    fn show_key_check(self: &Rc<Self>, statuses: Vec<(String, bool)>) {
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        let look_up = gtk::Button::with_mnemonic("_Look up missing keys");
        let back = gtk::Button::with_mnemonic("_Return to message");
        let send = gtk::Button::builder()
            .label("_Send")
            .use_underline(true)
            .sensitive(false)
            .hexpand(true)
            .css_classes(["suggested-action", "pill"])
            .build();
        let row = gtk::Box::builder().homogeneous(true).spacing(12).build();
        row.append(&look_up);
        row.append(&back);
        let note = gtk::Label::builder()
            .label("Encrypting needs a key for every recipient.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dimmed"])
            .build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        column.append(&note);
        column.append(&list);
        column.append(&row);
        column.append(&send);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&gtk::ScrolledWindow::builder().child(&column).propagate_natural_height(true).build()));
        let dialog = adw::Dialog::builder().title("Recipients without a key").content_width(440).child(&view).build();
        let fill = {
            let (list, send, look_up) = (list.clone(), send.clone(), look_up.clone());
            Rc::new(move |statuses: &[(String, bool)]| {
                list.remove_all();
                for (email, ok) in statuses {
                    let r = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(email))
                        .subtitle(if *ok { "Key found" } else { "No usable key" })
                        .build();
                    let icon = gtk::Image::from_icon_name(if *ok { "emblem-ok-symbolic" } else { "dialog-warning-symbolic" });
                    icon.add_css_class(if *ok { "success" } else { "warning" });
                    r.add_prefix(&icon);
                    list.append(&r);
                }
                let all = statuses.iter().all(|(_, ok)| *ok);
                send.set_sensitive(all);
                look_up.set_sensitive(!all);
            })
        };
        fill(&statuses);
        let statuses = Rc::new(RefCell::new(statuses));
        {
            let d = dialog.clone();
            back.connect_clicked(move |_| {
                d.close();
            });
        }
        {
            let (d, this) = (dialog.clone(), self.clone());
            send.connect_clicked(move |_| {
                d.close();
                this.transmit_composed();
            });
        }
        {
            let (this, d, fill, statuses) = (self.clone(), dialog.clone(), fill.clone(), statuses.clone());
            look_up.connect_clicked(move |b| {
                let missing: Vec<String> = statuses.borrow().iter().filter(|(_, ok)| !ok).map(|(e, _)| e.clone()).collect();
                b.set_sensitive(false);
                b.set_label("Looking up…");
                let (this, d, fill, statuses, b) = (this.clone(), d.clone(), fill.clone(), statuses.clone(), b.clone());
                glib::spawn_future_local(async move {
                    let keyserver = Settings::get().keyserver;
                    let found = this.find_keys(&missing, &keyserver).await;
                    b.set_label("_Look up missing keys");
                    b.set_sensitive(true);
                    let refresh = {
                        let (this, fill, statuses) = (this.clone(), fill.clone(), statuses.clone());
                        move || {
                            let (this, fill, statuses) = (this.clone(), fill.clone(), statuses.clone());
                            glib::spawn_future_local(async move {
                                let emails: Vec<String> = statuses.borrow().iter().map(|(e, _)| e.clone()).collect();
                                let fresh = this.key_statuses(&emails).await;
                                fill(&fresh);
                                *statuses.borrow_mut() = fresh;
                            });
                        }
                    };
                    this.confirm_import(&d, &missing, found, refresh);
                });
            });
        }
        dialog.present(Some(&self.window));
    }

    fn transmit_composed(self: &Rc<Self>) {
        let Some(account) = self.compose.borrow().as_ref().map(|c| c.account.clone()) else { return };
        // What a sent reply or forward marks on its original.
        let response = self.compose.borrow().as_ref().and_then(|c| c.original.clone().map(|o| (o, c.kind)));
        let out = self.composer.outgoing();
        // A draft reopened from Drafts knows its original only by
        // Message-ID: a forward's X-Forwarded-Message-Id, a reply's
        // In-Reply-To.
        let lookup = if response.is_none() {
            out.forwarded_id.clone().map(|id| (id, Kind::Forward)).or_else(|| out.in_reply_to.clone().map(|id| (id, Kind::Reply)))
        } else {
            None
        };
        self.composer.set_status(None);
        self.composer.set_sending(true);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let keys = this.composer_keys(&account);
            let result = send::send(&this.session, &account.account, &out, keys).await;
            this.composer.set_sending(false);
            let sent = match result {
                Ok(m) => m,
                Err(e) if send::is_invalid(&e) => {
                    this.composer.set_status(Some(&format!("Not sent: {}", e.message())));
                    return;
                }
                Err(e) => {
                    // Send commits the message: it goes out later.
                    if let Some((original, kind)) = &response {
                        mark_responded(original, *kind);
                    }
                    if let Some((id, kind)) = lookup {
                        this.mark_original(&account, id, kind);
                    }
                    this.queue_failed(account, out, send::describe_failure(&e));
                    return;
                }
            };
            // The message left; the composer closes before the follow-ups.
            let Some(state) = this.compose.borrow_mut().take() else { return };
            this.composer.mark_saved();
            this.return_from_composer(state.original.clone());
            this.toast("Message sent", false);
            if let Some(original) = &state.original {
                mark_responded(original, state.kind);
            }
            if let Some((id, kind)) = lookup {
                this.mark_original(&account, id, kind);
            }
            let draft = state.draft_uid.clone();
            let original = state.original.clone();
            drop(state);
            this.file_sent(&account, &sent).await;
            // Its line under the original's headers now has a date.
            if let Some((folder, uid)) = original
                && this.viewer.current().is_some_and(|(f, u)| f == folder && u == uid)
            {
                this.show_responses(&folder, &uid);
            }
            if let Some(uid) = draft {
                this.delete_draft(&account, uid).await;
            }
        });
    }

    /// Finds the message with Message-ID `id` in the account (INBOX first,
    /// then Archive, then the other folders but Drafts, Sent, Junk and
    /// Trash) and marks it as `kind` answered it.
    fn mark_original(self: &Rc<Self>, account: &Rc<AccountCtl>, id: String, kind: Kind) {
        let Some(store) = account.store.borrow().clone() else { return };
        let special = account.special.borrow().clone().unwrap_or_default();
        let skip: Vec<String> = [&special.drafts, &special.sent, &special.junk, &special.trash].into_iter().flatten().cloned().collect();
        let mut names: Vec<String> = vec!["INBOX".into()];
        names.extend(special.archive.clone());
        fn walk(store: &gio::ListStore, out: &mut Vec<String>) {
            for i in 0..store.n_items() {
                if let Some(o) = store.item(i).and_downcast::<FolderObject>() {
                    out.push(o.full_name());
                    walk(o.children(), out);
                }
            }
        }
        let mut rest = Vec::new();
        walk(account.obj.children(), &mut rest);
        for n in rest {
            if !names.contains(&n) && !skip.contains(&n) {
                names.push(n);
            }
        }
        let this = self.clone();
        let quoted = id.replace('\\', "\\\\").replace('"', "\\\"");
        glib::spawn_future_local(async move {
            for name in names {
                let Ok(folder) = this.session.folder_future(&store, &name).await else { continue };
                let (f, expr) = (folder.clone(), format!("(match-all (header-contains \"Message-ID\" \"{quoted}\"))"));
                let uids = gio::spawn_blocking(move || f.search_sync(&expr, None::<&gio::Cancellable>).unwrap_or_default())
                    .await
                    .unwrap_or_default();
                if let Some(uid) = uids.into_iter().next() {
                    crate::debug::log(&format!("draft's original found in {name}"));
                    let original = (folder, uid.to_string());
                    mark_responded(&original, kind);
                    return;
                }
            }
            crate::debug::log("draft's original not found");
        });
    }

    /// Appends a sent message to the account's Sent folder.
    pub(super) async fn file_sent(self: &Rc<Self>, account: &Rc<AccountCtl>, msg: &camel::MimeMessage) {
        let sent = account.special.borrow().as_ref().and_then(|s| s.sent.clone());
        let store = account.store.borrow().clone();
        let (Some(sent), Some(store)) = (sent, store) else { return };
        let result = match self.session.folder_future(&store, &sent).await {
            Ok(folder) => send::append(&folder, msg, SEEN).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            self.toast(&format!("Sent, but not saved to Sent: {}", e.message()), false);
        }
    }

    /// The account's key and whether it goes in an Autocrypt header.
    fn composer_keys(&self, account: &AccountCtl) -> Option<send::Keys> {
        self.account_keys(account)
    }

    /// The account's key and its Autocrypt setting, also for messages sent
    /// without the composer (the send queue). An attached key is one of
    /// the message's attachments.
    pub(super) fn account_keys(&self, account: &AccountCtl) -> Option<send::Keys> {
        let prefs = Settings::get().compose_for(&account.account.uid);
        let signer = prefs.signing_key?;
        Some(send::Keys { signer, autocrypt: prefs.autocrypt })
    }

    /// The decrypted content of an encrypted message, else None.
    pub(super) async fn decrypt(&self, msg: &camel::MimeMessage) -> Option<camel::MimePart> {
        struct Handoff<T>(T);
        unsafe impl<T> Send for Handoff<T> {}
        let job = Handoff((self.gpg.clone(), msg.clone()));
        gio::spawn_blocking(move || {
            let Handoff((ctx, msg)) = job;
            Handoff(crypto::unprotect_sync(&ctx, &msg).filter(|(_, p)| p.encrypted && p.decrypt_error.is_none()).map(|(b, _)| b))
        })
        .await
        .ok()
        .and_then(|h| h.0)
    }

    /// Looks the addresses' keys up by WKD and on the keyserver: (address,
    /// key data, fingerprint, user IDs) of each key found.
    async fn find_keys(&self, missing: &[String], keyserver: &str) -> Vec<(String, Vec<u8>, String, Vec<String>)> {
        let mut found = Vec::new();
        for addr in missing {
            let Some(data) = crypto::lookup(addr, keyserver).await else { continue };
            let (ctx, d) = (self.gpg.clone(), data.clone());
            let infos = gio::spawn_blocking(move || ctx.key_data_info_sync(&d).unwrap_or_default()).await.unwrap_or_default();
            if let Some(k) = infos.into_iter().find(|k| crypto::key_has_address(k, addr)) {
                found.push((addr.clone(), data, k.fingerprint, k.user_ids));
            }
        }
        found
    }

    /// Shows the keys found with their fingerprints and imports them on
    /// confirmation; `done` runs after an import.
    fn confirm_import(
        self: &Rc<Self>,
        parent: &impl IsA<gtk::Widget>,
        missing: &[String],
        found: Vec<(String, Vec<u8>, String, Vec<String>)>,
        done: impl Fn() + 'static,
    ) {
        let not_found: Vec<&String> = missing.iter().filter(|a| !found.iter().any(|f| &f.0 == *a)).collect();
        if found.is_empty() {
            let dialog = adw::AlertDialog::new(Some("No keys found"), Some("None of the missing keys could be found."));
            dialog.add_response("close", "_Close");
            dialog.present(Some(parent));
            return;
        }
        let mut text = String::from("Check each fingerprint with its owner if you can.\n");
        for (addr, _, fpr, uids) in &found {
            text.push_str(&format!("\n{addr}\n{}\n{}\n", uids.join(", "), group_fingerprint(fpr)));
        }
        if !not_found.is_empty() {
            text.push_str(&format!("\nNot found: {}", not_found.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        let dialog = adw::AlertDialog::new(Some("Import these keys?"), Some(&text));
        dialog.add_responses(&[("cancel", "_Cancel"), ("import", "_Import")]);
        dialog.set_response_appearance("import", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        let this = self.clone();
        let done = Rc::new(done);
        dialog.connect_response(None, move |_, response| {
            if response != "import" {
                return;
            }
            let (this, done) = (this.clone(), done.clone());
            let datas: Vec<Vec<u8>> = found.iter().map(|f| f.1.clone()).collect();
            glib::spawn_future_local(async move {
                let ctx = this.gpg.clone();
                let result = gio::spawn_blocking(move || {
                    for d in datas {
                        ctx.import_key_sync(&d)?;
                    }
                    Ok::<(), glib::Error>(())
                })
                .await;
                if let Ok(Err(e)) = result {
                    this.toast(&format!("Cannot import the keys: {}", e.message()), false);
                }
                done();
            });
        });
        dialog.present(Some(parent));
    }

    /// After Send or Discard: back to the message replied to or forwarded
    /// while it is still where it was, otherwise to its folder's list; for a
    /// new message, an empty view (on a phone, the list).
    fn return_from_composer(self: &Rc<Self>, original: Option<(camel::Folder, String)>) {
        const DELETED: u32 = camel::MessageFlags::DELETED.bits();
        self.show_viewer();
        let Some((folder, uid)) = original else {
            if self.inner.is_collapsed() {
                self.inner.set_show_content(false);
            }
            return;
        };
        if folder.message_info(&uid).is_none() || folder.message_flags(&uid) & DELETED != 0 {
            // Gone: its folder's list instead.
            if self.list.folder().as_ref() != Some(&folder)
                && let Some(obj) = self.account_of(&folder).and_then(|a| a.obj.find(&folder.full_name().unwrap_or_default()))
            {
                self.folders.select(&obj);
            }
            if self.inner.is_collapsed() {
                self.inner.set_show_content(false);
            }
            return;
        }
        if self.list.folder().as_ref() == Some(&folder) && !self.showing_results() {
            self.list.open_uid(&uid);
        } else {
            self.viewer.show(folder, uid);
            self.update_viewer_read();
        }
    }

    pub(super) fn show_viewer(&self) {
        self.content_stack.set_visible_child_name("viewer");
        self.viewer.clear();
        self.update_viewer_read();
    }

    /// Shows Reply and Reply All as menus (Plain Text or Markdown) when the
    /// shown message is HTML and its account writes plain text.
    pub(super) fn update_reply_bar(&self) {
        let shown = self.viewer.current();
        self.viewer_view.set_reveal_bottom_bars(shown.is_some());
        let choose = shown
            .and_then(|(folder, _)| self.account_of(&folder))
            .is_some_and(|a| Settings::get().compose_for(&a.account.uid).format == Format::Plain)
            && self.viewer.is_html();
        let page = if choose { "menu" } else { "button" };
        self.reply_stack.set_visible_child_name(page);
        self.reply_all_stack.set_visible_child_name(page);
    }
}
