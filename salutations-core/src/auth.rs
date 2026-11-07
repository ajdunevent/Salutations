//! Credentials for Camel services, taken from the EDS source registry.
//! GOA-backed sources get OAuth2 tokens from the registry and passwords
//! from EDS's GOA credentials module; nothing is stored here.

use camel::prelude::*;
use edataserver::prelude::*;
use edataserver::{
    E_SOURCE_CREDENTIAL_PASSWORD, E_SOURCE_EXTENSION_AUTHENTICATION,
    E_SOURCE_EXTENSION_COLLECTION, Source, SourceAuthentication, SourceCredentialsReason,
};

use crate::session::imp::Session;

fn cant_authenticate(message: &str) -> glib::Error {
    glib::Error::new(camel::ServiceError::CantAuthenticate, message)
}

fn source_for(session: &Session, service: &camel::Service) -> Result<Source, glib::Error> {
    let uid = service.uid().unwrap_or_default();
    session
        .registry()
        .ref_source(&uid)
        .ok_or_else(|| cant_authenticate(&format!("No data source found for UID “{uid}”")))
}

pub(crate) fn authenticate_sync(
    session: &Session,
    service: &camel::Service,
    mechanism: Option<&str>,
    cancellable: Option<&gio::Cancellable>,
) -> Result<(), glib::Error> {
    let mechanism = mechanism.filter(|m| *m != "none");

    // XOAUTH2 and OAUTHBEARER fetch their token through
    // oauth2_access_token_sync() inside this call.
    if let Some(m) = mechanism
        && camel::sasl_mechanism_needs_password(m) == Some(false)
    {
        return accepted(service.authenticate_sync(mechanism, cancellable)?, mechanism);
    }

    if let Some(m) = mechanism {
        let provider = service.provider();
        let protocol = provider.as_ref().and_then(|p| p.protocol());
        let sasl = protocol.and_then(|p| camel::Sasl::new(&p, m, service));
        if let Some(sasl) = sasl
            && sasl.try_empty_password_sync(cancellable)?
            && service.authenticate_sync(mechanism, cancellable)? == camel::AuthenticationResult::Accepted
        {
            return Ok(());
        }
    }

    let source = source_for(session, service)?;
    let password = match session.credentials().lookup_sync(&source, cancellable) {
        Ok(credentials) => {
            let credential_name = source
                .extension::<SourceAuthentication>(E_SOURCE_EXTENSION_AUTHENTICATION)
                .and_then(|auth| auth.credential_name())
                .filter(|name| !name.is_empty());
            credentials
                .get(credential_name.as_deref().unwrap_or(E_SOURCE_CREDENTIAL_PASSWORD))
                .ok_or_else(|| cant_authenticate("No password available for this account"))?
                .to_string()
        }
        // Before giving a password, EDS has GOA check the account by
        // logging in to both IMAP and SMTP. A certificate GOA was told to
        // ignore for one of them fails that check when the other's flag is
        // off, and then neither gets a password. The password is taken
        // from GOA directly when this service's own flag allows the
        // certificate; the connection applies that same flag.
        Err(e) if crate::trust::is_goa_certificate_error(&e) => {
            let registry = session.registry();
            let smtp = service.is::<camel::Transport>();
            if crate::trust::accepts_errors(registry, service, smtp) != Some(true) {
                return Err(e);
            }
            glib::g_info!("salutations", "GOA's account check failed on a certificate; asking GOA for the password directly");
            crate::trust::goa_password(registry, service, smtp).ok_or(e)?
        }
        Err(e) => return Err(e),
    };
    service.set_password(&password);

    accepted(service.authenticate_sync(mechanism, cancellable)?, mechanism)
}

fn accepted(result: camel::AuthenticationResult, mechanism: Option<&str>) -> Result<(), glib::Error> {
    match result {
        camel::AuthenticationResult::Accepted => Ok(()),
        _ => Err(cant_authenticate(&format!(
            "{} authentication failed",
            mechanism.unwrap_or("Password")
        ))),
    }
}

pub(crate) fn oauth2_access_token_sync(
    session: &Session,
    service: &camel::Service,
    cancellable: Option<&gio::Cancellable>,
) -> Result<(glib::GString, i32), glib::Error> {
    let registry = session.registry();
    let source = source_for(session, service)?;
    // GOA accounts hold the token on the collection source.
    let cred_source = registry
        .find_extension(&source, E_SOURCE_EXTENSION_COLLECTION)
        .filter(|c| edataserver::util_can_use_collection_as_credential_source(Some(c), &source))
        .unwrap_or(source);

    cred_source.oauth2_access_token_sync(cancellable).map_err(|e| {
        // EDS reports an expired or unrefreshable token with these codes;
        // Evolution's EMailSession maps them to CantAuthenticate for Camel.
        if e.matches(gio::IOErrorEnum::ConnectionRefused) || e.matches(gio::IOErrorEnum::NotFound) {
            let e = cant_authenticate(e.message());
            let _ = cred_source.invoke_credentials_required_sync(
                SourceCredentialsReason::Rejected,
                "",
                gio::TlsCertificateFlags::empty(),
                Some(&e),
                cancellable,
            );
            e
        } else {
            e
        }
    })
}
