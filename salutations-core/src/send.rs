//! Sending through the account's SMTP transport, and saving to Drafts and
//! Sent. The blocking calls run on worker threads through `spawn_blocking`.

use camel::prelude::*;
use edataserver::prelude::*;

use crate::compose::Outgoing;
use crate::{Account, Session};

/// IMAP flags of a saved draft.
pub const DRAFT_FLAGS: u32 = camel::MessageFlags::DRAFT.bits() | camel::MessageFlags::SEEN.bits();

fn failed(message: &str) -> glib::Error {
    glib::Error::new(camel::ServiceError::Unavailable, message)
}

fn panicked(_: Box<dyn std::any::Any + Send>) -> glib::Error {
    failed("The sending thread stopped unexpectedly")
}

impl Session {
    /// The account's SMTP transport, created on first use.
    pub fn transport(&self, account: &Account) -> Result<camel::Transport, glib::Error> {
        let uid = account
            .transport_uid()
            .ok_or_else(|| failed("This account has no outgoing mail server set up in Online Accounts"))?;
        let session = self.upcast_ref::<camel::Session>();
        let service = match session.ref_service(&uid) {
            Some(s) => s,
            None => {
                let backend = self
                    .registry()
                    .ref_source(&uid)
                    .and_then(|s| s.extension::<edataserver::SourceMailTransport>(edataserver::E_SOURCE_EXTENSION_MAIL_TRANSPORT))
                    .and_then(|t| t.backend_name())
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "smtp".into());
                session.add_service(&uid, &backend, camel::ProviderType::Transport)?
            }
        };
        service
            .downcast::<camel::Transport>()
            .map_err(|_| failed("The outgoing mail server is not a mail transport"))
    }
}

/// A message that cannot be sent as written: no recipients or an address
/// that does not parse. Sending it again cannot help.
pub fn is_invalid(e: &glib::Error) -> bool {
    e.matches(gio::IOErrorEnum::InvalidArgument)
}

/// Why a send failed, for the user. A refused certificate means GNOME
/// Online Accounts was not told to ignore the outgoing server's
/// certificate, which only GOA can change.
pub fn describe_failure(e: &glib::Error) -> String {
    if e.matches(gio::TlsError::BadCertificate) {
        return "The outgoing server's certificate is not trusted, and Online Accounts is not set to ignore that for this account".into();
    }
    e.message().to_string()
}

fn invalid(message: &str) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::InvalidArgument, message)
}

/// Sends `out` and returns the message as sent, for Sent. Refuses a
/// message without recipients or with an address that does not parse
/// ([`is_invalid`]).
pub async fn send(session: &Session, account: &Account, out: &Outgoing, keys: Option<Keys>) -> Result<camel::MimeMessage, glib::Error> {
    let recipients = out.recipients().map_err(|e| invalid(&format!("“{}” is not a valid address", e.0)))?;
    if recipients.length() == 0 {
        return Err(invalid("Add at least one recipient"));
    }
    out.headers(false).map_err(|e| invalid(&format!("“{}” is not a valid address", e.0)))?;
    // Signing and encryption failures (a missing key, a cancelled
    // passphrase) are the message's problem, not the server's.
    let msg = prepare(session, out, keys, false).await.map_err(|e| invalid(e.message()))?;
    transmit(session, account, &msg, out.from_address(), recipients).await?;
    Ok(msg)
}

/// The account's key and how it is shared, for [`send`] and [`draft`].
#[derive(Debug, Clone)]
pub struct Keys {
    /// The signing key's fingerprint.
    pub signer: String,
    /// Send it in an Autocrypt header.
    pub autocrypt: bool,
}

/// Carries values that are only touched by one thread at a time across
/// `spawn_blocking`; Camel objects are not marked Send.
struct Handoff<T>(T);
unsafe impl<T> Send for Handoff<T> {}

/// The message to send, or (`draft`) to save in Drafts, with the key
/// shared and the content signed and encrypted as `out` asks. A draft is
/// never signed; with Encrypt on it is encrypted to the sender's own key.
async fn prepare(session: &Session, out: &Outgoing, keys: Option<Keys>, draft: bool) -> Result<camel::MimeMessage, glib::Error> {
    let Some(keys) = keys else {
        return out.build(draft).map_err(|e| invalid(&e.0));
    };
    let ctx = crate::crypto::context(session);
    let job = Handoff((out.clone(), ctx));
    gio::spawn_blocking(move || {
        let Handoff((mut out, ctx)) = job;
        if !draft && keys.autocrypt {
            let key = ctx.public_key_sync(&keys.signer)?;
            out.autocrypt = Some(crate::crypto::autocrypt_header(&out.from.email, &key));
        }
        let msg = out.headers(draft).map_err(|e| invalid(&e.0))?;
        let content = out.content_part();
        if draft {
            if out.encrypt {
                crate::crypto::protect_sync(&ctx, &msg, &content, None, Some(std::slice::from_ref(&keys.signer)))?;
            } else {
                crate::compose::set_body(&msg, &content);
            }
        } else {
            let to: Vec<String> = out.recipient_emails();
            crate::crypto::protect_sync(
                &ctx,
                &msg,
                &content,
                out.sign.then_some(keys.signer.as_str()),
                out.encrypt.then_some(to.as_slice()),
            )?;
        }
        Ok(Handoff(msg))
    })
    .await
    .map_err(panicked)?
    .map(|h| h.0)
}

/// The message to save in Drafts; see [`prepare`].
pub async fn draft(session: &Session, out: &Outgoing, keys: Option<Keys>) -> Result<camel::MimeMessage, glib::Error> {
    prepare(session, out, keys, true).await
}

async fn transmit(
    session: &Session,
    account: &Account,
    msg: &camel::MimeMessage,
    from: camel::InternetAddress,
    recipients: camel::InternetAddress,
) -> Result<(), glib::Error> {
    let transport = session.transport(account)?;
    let (m, t) = (msg.clone(), transport.clone());
    gio::spawn_blocking(move || {
        if t.connection_status() != camel::ServiceConnectionStatus::Connected {
            t.connect_sync(None::<&gio::Cancellable>)?;
        }
        let result = t.send_to_sync(&m, &from, &recipients, None::<&gio::Cancellable>);
        // A transport is not kept connected between messages.
        let _ = t.disconnect_sync(true, None::<&gio::Cancellable>);
        result
    })
    .await
    .map_err(panicked)??;
    Ok(())
}

/// Appends `msg` to `folder` with `flags`; returns the new UID when the
/// server reports it.
pub async fn append(folder: &camel::Folder, msg: &camel::MimeMessage, flags: u32) -> Result<Option<String>, glib::Error> {
    let (f, m) = (folder.clone(), msg.clone());
    gio::spawn_blocking(move || f.append_message_sync(&m, flags, None::<&gio::Cancellable>))
        .await
        .map_err(panicked)?
}
