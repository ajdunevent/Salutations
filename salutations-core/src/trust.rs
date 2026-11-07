//! Certificates that fail validation, such as Proton Mail Bridge's
//! self-signed one on 127.0.0.1. Salutations decides as GNOME Online
//! Accounts did when the account was added: GOA validates the IMAP
//! and SMTP certificates then, offers "Ignore" when one fails, and records
//! that choice per account as `ImapAcceptSslErrors` and
//! `SmtpAcceptSslErrors`. Salutations accepts a failing certificate only
//! when the account's flag for that server is set, and never asks itself.

use camel::prelude::*;
use edataserver::prelude::*;
use edataserver::{E_SOURCE_EXTENSION_GOA, SourceGoa, SourceRegistry};

/// Camel's answer for a certificate of `service` that failed validation.
/// Camel stores every answer in its certificate database and asks again
/// only about an "unknown" one, so a refusal is "unknown": turning the
/// flag on in GOA takes effect at the next connection. An acceptance is
/// "temporary", and `forget_answers` drops it at the next start, so
/// turning the flag off takes effect then.
pub fn decide(registry: &SourceRegistry, service: &camel::Service, errors: gio::TlsCertificateFlags) -> camel::ffi::CamelCertTrust {
    let smtp = service.is::<camel::Transport>();
    let accepted = accepts_errors(registry, service, smtp).unwrap_or(false);
    let uid = service.uid().map(|u| u.to_string()).unwrap_or_default();
    glib::g_info!(
        "salutations",
        "{} certificate for {uid} failed validation ({errors:?}); {}",
        if smtp { "SMTP" } else { "IMAP" },
        if accepted { "accepted, as the GOA account allows" } else { "refused" }
    );
    if accepted { camel::ffi::CAMEL_CERT_TRUST_TEMPORARY } else { camel::ffi::CAMEL_CERT_TRUST_UNKNOWN }
}

fn goa_account_id(registry: &SourceRegistry, service: &camel::Service) -> Option<String> {
    let source = registry.ref_source(service.uid()?.as_str())?;
    let goa = registry.find_extension(&source, E_SOURCE_EXTENSION_GOA)?;
    goa.extension::<SourceGoa>(E_SOURCE_EXTENSION_GOA)?.account_id().map(|s| s.to_string())
}

/// The GOA account's `ImapAcceptSslErrors` or `SmtpAcceptSslErrors`.
pub(crate) fn accepts_errors(registry: &SourceRegistry, service: &camel::Service, smtp: bool) -> Option<bool> {
    let id = goa_account_id(registry, service)?;
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).ok()?;
    let property = if smtp { "SmtpAcceptSslErrors" } else { "ImapAcceptSslErrors" };
    let reply = bus
        .call_sync(
            Some("org.gnome.OnlineAccounts"),
            &format!("/org/gnome/OnlineAccounts/Accounts/{id}"),
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.gnome.OnlineAccounts.Mail", property).to_variant()),
            Some(glib::VariantTy::new("(v)").ok()?),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    let (value,): (glib::Variant,) = reply.get()?;
    value.get::<bool>()
}

/// Whether `e` is GOA's "invalid certificate" error, as EDS passes on a
/// failed credentials check.
pub(crate) fn is_goa_certificate_error(e: &glib::Error) -> bool {
    // GOA_ERROR_SSL, or its D-Bus name when the domain is not registered.
    (e.domain().as_str() == "goa-error-quark" && e.code() == 5)
        || gio::DBusError::remote_error(e).is_some_and(|n| n == "org.gnome.OnlineAccounts.Error.SSL")
}

/// The password GOA stores for `service`, asked for directly. GOA's
/// PasswordBased interface returns it without the account check that
/// EDS's lookup runs first.
pub(crate) fn goa_password(registry: &SourceRegistry, service: &camel::Service, smtp: bool) -> Option<String> {
    let id = goa_account_id(registry, service)?;
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).ok()?;
    let which = if smtp { "smtp-password" } else { "imap-password" };
    let reply = bus
        .call_sync(
            Some("org.gnome.OnlineAccounts"),
            &format!("/org/gnome/OnlineAccounts/Accounts/{id}"),
            "org.gnome.OnlineAccounts.PasswordBased",
            "GetPassword",
            Some(&(which,).to_variant()),
            Some(glib::VariantTy::new("(s)").ok()?),
            gio::DBusCallFlags::NONE,
            10_000,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    let (password,): (String,) = reply.get()?;
    Some(password)
}

/// Empties Camel's certificate database, so each run asks the GOA flags
/// again. It holds only Salutations' own answers. Call after Camel's init.
pub fn forget_answers() {
    unsafe {
        let db = camel::ffi::camel_certdb_get_default();
        if db.is_null() {
            return;
        }
        camel::ffi::camel_certdb_clear(db);
        camel::ffi::camel_certdb_save(db);
        glib::gobject_ffi::g_object_unref(db as *mut _);
    }
}
