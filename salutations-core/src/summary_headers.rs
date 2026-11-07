//! Camel keeps up to three headers of the user's choosing in each folder
//! summary, named in the `camel-message-info-user-headers` setting of
//! `org.gnome.evolution-data-server`. The message list needs `Importance`
//! and `X-Priority` for the importance mark, and `Authentication-Results`
//! to show a BIMI logo only on verified messages, so they are added there
//! in that order.
//!
//! The setting is shared with Evolution outside a sandbox; names already
//! in it are kept, and nothing is added once it holds three. Only messages
//! fetched after a header is added carry it.

use gio::prelude::*;

const SCHEMA: &str = "org.gnome.evolution-data-server";
const KEY: &str = "camel-message-info-user-headers";
/// CAMEL_UTILS_MAX_USER_HEADERS.
const MAX: usize = 3;
const WANTED: [&str; 3] = ["Importance", "X-Priority", "Authentication-Results"];

/// The header name in an entry, which may be "Display Name|Header".
fn header_of(entry: &str) -> &str {
    entry.rsplit_once('|').map_or(entry, |(_, h)| h).trim()
}

/// `current` with the wanted headers appended while there is room.
fn merged(current: &[String]) -> Vec<String> {
    let mut out = current.to_vec();
    for h in WANTED {
        if out.iter().any(|e| header_of(e).eq_ignore_ascii_case(h)) {
            continue;
        }
        if out.iter().filter(|e| !header_of(e).is_empty()).count() >= MAX {
            break;
        }
        out.push(h.to_string());
    }
    out
}

pub fn ensure() {
    let Some(source) = gio::SettingsSchemaSource::default() else { return };
    let Some(schema) = source.lookup(SCHEMA, true) else { return };
    if !schema.has_key(KEY) {
        return;
    }
    let settings = gio::Settings::new(SCHEMA);
    let current: Vec<String> = settings.strv(KEY).iter().map(|s| s.to_string()).collect();
    let new = merged(&current);
    if new != current {
        let refs: Vec<&str> = new.iter().map(String::as_str).collect();
        if let Err(e) = settings.set_strv(KEY, refs.as_slice()) {
            glib::g_warning!("salutations", "Cannot set {KEY}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merging() {
        assert_eq!(merged(&[]), v(&["Importance", "X-Priority", "Authentication-Results"]));
        assert_eq!(merged(&v(&["Spam|X-Spam-Score"])), v(&["Spam|X-Spam-Score", "Importance", "X-Priority"]));
        assert_eq!(merged(&v(&["a", "b"])), v(&["a", "b", "Importance"]));
        assert_eq!(merged(&v(&["Prio|x-priority"])), v(&["Prio|x-priority", "Importance", "Authentication-Results"]));
        assert_eq!(merged(&v(&["a", "b", "c"])), v(&["a", "b", "c"]));
    }
}
