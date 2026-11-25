//! Mail engine of Salutations. Camel does the IMAP and SMTP work; this
//! crate holds the session, account discovery, and credential glue. It
//! must not depend on GTK so it can move into a separate daemon.

mod accounts;
pub mod allowed;
mod auth;
pub mod compose;
pub mod avatars;
pub mod contacts;
pub mod crypto;
pub mod debug;
pub mod downsync;
pub mod engine;
pub mod hidden;
pub mod ipc;
pub mod links;
pub mod mailto;
pub mod offline;
pub mod origins;
pub mod outbox;
pub mod push;
pub mod remote;
pub mod send;
pub mod summary_headers;
pub mod trust;
pub mod xface;
pub mod render;
pub mod rules;
mod session;
pub mod settings;
pub mod verify;

use std::path::PathBuf;

pub use accounts::{Account, mail_accounts, watch_mail_accounts};
pub use render::{Address, Rendered};
pub use session::{Role, Session, SpecialFolders};
pub use settings::{AttachKey, ComposePrefs, DEFAULT_KEYSERVER, ImportKeys, Settings, SyncPrefs};

pub use camel;
pub use edataserver;

pub const APP_ID: &str = "net.dunevent.Salutations";

/// Camel's state: account folders, summaries, and the NSS certificate
/// database.
pub fn data_dir() -> PathBuf {
    glib::user_data_dir().join("salutations")
}

/// Cached message bodies.
pub fn cache_dir() -> PathBuf {
    glib::user_cache_dir().join("salutations")
}

/// Initializes Camel for the window process, which only parses messages
/// and runs OpenPGP; the background process owns the stores and the
/// certificate database.
pub fn init_window() -> Result<(), glib::Error> {
    camel::init(data_dir(), false).map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))
}

/// Initializes Camel and loads its providers. Must run once, on the main
/// thread, before a `Session` or `SourceRegistry` is created.
pub fn init() -> Result<(), glib::Error> {
    let data_dir = data_dir();
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        glib::Error::new(
            gio::IOErrorEnum::Failed,
            &format!("Cannot create {}: {e}", data_dir.display()),
        )
    })?;
    summary_headers::ensure();
    camel::init(&data_dir, true)
        .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))?;
    trust::forget_answers();
    if !camel::load_providers().iter().any(|p| p == "imapx") {
        return Err(glib::Error::new(
            gio::IOErrorEnum::NotFound,
            "Camel's IMAP provider (imapx) is not installed",
        ));
    }
    Ok(())
}

/// `path` after the folder `old` was renamed or moved to `new`: `path`
/// itself or one of its parents. None when `path` is not under `old`.
pub fn renamed_path(path: &str, old: &str, new: &str) -> Option<String> {
    if path == old {
        return Some(new.to_string());
    }
    path.strip_prefix(old).and_then(|rest| rest.strip_prefix('/')).map(|rest| format!("{new}/{rest}"))
}

/// Whether `path` is the folder `folder` or inside it.
pub fn within(path: &str, folder: &str) -> bool {
    path == folder || path.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn folder_paths() {
        assert_eq!(super::renamed_path("A/B/C", "A/B", "X/B").as_deref(), Some("X/B/C"));
        assert_eq!(super::renamed_path("A/B", "A/B", "Y").as_deref(), Some("Y"));
        assert_eq!(super::renamed_path("A/Bee", "A/B", "Y"), None);
        assert!(super::within("A/B", "A") && !super::within("AB", "A"));
    }
}
