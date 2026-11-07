//! Holds off Camel's own downloads for offline use. Camel starts them as
//! session jobs (new arrivals in a folder kept offline, and the whole
//! account when it goes online), gated only by settings that are saved
//! with the account. Instead of changing those, the jobs of a held store
//! are cancelled as they start, recognized by the message Camel gives
//! them.

use std::sync::Mutex;

use camel::prelude::*;
use glib::translate::*;

/// Display names of the stores whose downloads are held.
static HELD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Camel's job messages, untranslated, with the store's display name as
/// the first "%s".
const TEMPLATES: [&str; 2] = [
    "Checking download of new messages for offline in “%s : %s”",
    "Syncing messages in account “%s” to disk",
];

const DOMAIN: &str = "evolution-data-server";

pub fn hold(store: &camel::Store, held: bool) {
    let name = store.display_name().map(|n| n.to_string()).unwrap_or_default();
    let mut list = HELD.lock().unwrap();
    list.retain(|n| *n != name);
    if held {
        list.push(name);
    }
}

/// Watches `session`'s jobs. Call once.
pub fn install(session: &impl IsA<camel::Session>) {
    session.connect_job_started(|_, cancellable| {
        let Some(c) = cancellable else { return };
        let message: Option<String> =
            unsafe { from_glib_full(camel::ffi::camel_operation_dup_message(c.to_glib_none().0)) };
        let Some(message) = message else { return };
        if HELD.lock().unwrap().iter().any(|name| is_download(&message, name)) {
            c.cancel();
        }
    });
}

/// Whether `message` is one of the download jobs of the store named `name`.
fn is_download(message: &str, name: &str) -> bool {
    TEMPLATES.iter().any(|t| {
        let translated: String = unsafe { from_glib_none(glib::ffi::g_dgettext(DOMAIN.to_glib_none().0, t.to_glib_none().0)) };
        [translated.as_str(), t].iter().any(|t| matches(message, t, name))
    })
}

/// Whether `message` is `template` with `name` for its first "%s"; the
/// rest of the message is the folder and may be anything.
fn matches(message: &str, template: &str, name: &str) -> bool {
    let Some((before, after)) = template.split_once("%s") else { return false };
    let after = after.split("%s").next().unwrap_or("");
    message.strip_prefix(before).and_then(|m| m.strip_prefix(name)).is_some_and(|m| m.starts_with(after))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes() {
        let t = TEMPLATES[0];
        assert!(matches("Checking download of new messages for offline in “Work : INBOX”", t, "Work"));
        assert!(!matches("Checking download of new messages for offline in “Work 2 : INBOX”", t, "Work"));
        assert!(matches("Syncing messages in account “Work” to disk", TEMPLATES[1], "Work"));
        assert!(!matches("Syncing messages in account “Work 2” to disk", TEMPLATES[1], "Work"));
    }
}
