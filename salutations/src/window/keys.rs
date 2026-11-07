//! Keys that messages carry for their senders (Autocrypt headers and
//! attached `application/pgp-keys`), imported as the user's setting says.
//! Only a trusted message counts: one signed by a key already held for its
//! sender, or one whose provider recorded a DMARC pass for the sender's
//! domain. A key that differs from the one already held always asks.

use salutations_core::crypto::{self, CarriedKey, SignStatus};
use salutations_core::{ImportKeys, verify};

use super::*;

/// Carries values across `spawn_blocking`; only one thread touches them at
/// a time.
struct Handoff<T>(T);
unsafe impl<T> Send for Handoff<T> {}

impl Window {
    /// Looks at the shown message for a key to offer or import.
    pub(super) fn consider_keys(self: &Rc<Self>, folder: &camel::Folder) {
        let policy = Settings::get().import_keys;
        if policy == ImportKeys::Never {
            return;
        }
        let Some(msg) = self.viewer.message() else { return };
        let body = self.viewer.body();
        let carried = crypto::carried_keys(&msg, body.as_ref());
        let Some(key) = carried.into_iter().next() else { return };
        if !self.trusted(&msg, &key.address) {
            return;
        }
        let sender = msg
            .from()
            .and_then(|f| f.get(0))
            .and_then(|(n, e)| n.map(|n| n.to_string()).filter(|n| !n.is_empty()).or(e.map(|e| e.to_string())))
            .unwrap_or_else(|| key.address.clone());
        let shown = self.viewer.current();
        let (this, ctx, folder) = (self.clone(), self.gpg.clone(), folder.clone());
        glib::spawn_future_local(async move {
            let job = Handoff((ctx, key));
            let found = gio::spawn_blocking(move || {
                let Handoff((ctx, key)) = job;
                let offered = ctx
                    .key_data_info_sync(&key.data)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|k| crypto::key_has_address(k, &key.address))?;
                let held: Vec<String> = ctx
                    .public_key_info_sync(&key.address)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|k| crypto::key_has_address(k, &key.address))
                    .map(|k| k.fingerprint)
                    .collect();
                Some(Handoff((key, offered.fingerprint, held)))
            })
            .await
            .ok()
            .flatten();
            let Some(Handoff((key, fingerprint, held))) = found else { return };
            if held.contains(&fingerprint) {
                return;
            }
            // The user moved on to another message meanwhile.
            if this.viewer.current() != shown || this.viewer.current().is_none_or(|(f, _)| f != folder) {
                return;
            }
            let replace = !held.is_empty();
            if policy == ImportKeys::Always && !replace {
                this.import_carried(key, sender, false);
                return;
            }
            let text = if replace {
                format!("This message includes a new encryption key for {sender}.")
            } else {
                format!("This message includes {sender}'s encryption key.")
            };
            let weak = Rc::downgrade(&this);
            let s = sender.clone();
            this.viewer.offer_key(&text, if replace { "Replace" } else { "Import" }, move || {
                if let Some(this) = weak.upgrade() {
                    this.import_carried(key.clone(), s.clone(), true);
                }
            });
        });
    }

    /// Signed by a key already held for `address`, or a DMARC pass for the
    /// sender's domain recorded by the provider.
    fn trusted(&self, msg: &camel::MimeMessage, address: &str) -> bool {
        let signed = self.viewer.protection().and_then(|p| p.signature).is_some_and(|s| {
            matches!(s.status, SignStatus::Good | SignStatus::Unknown)
                && s.signer.and_then(|x| x.email).is_some_and(|e| e.eq_ignore_ascii_case(address))
        });
        let domain = address.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase());
        let dmarc = msg.header("Authentication-Results").map(|h| verify::parse(&h)).is_some_and(|v| {
            v.dmarc.as_deref() == Some("pass") && v.from_domain.map(|d| d.to_ascii_lowercase()) == domain
        });
        signed || dmarc
    }

    fn import_carried(self: &Rc<Self>, key: CarriedKey, sender: String, asked: bool) {
        let (this, ctx) = (self.clone(), self.gpg.clone());
        glib::spawn_future_local(async move {
            let data = key.data.clone();
            let result = gio::spawn_blocking(move || ctx.import_key_sync(&data)).await;
            match result {
                Ok(Ok(())) if asked => this.toast(&format!("Imported {sender}'s key"), false),
                Ok(Ok(())) => {}
                Ok(Err(e)) => this.toast(&format!("Cannot import the key: {}", e.message()), false),
                Err(_) => {}
            }
        });
    }
}
