//! Where messages moved to Trash came from, for Restore. Kept in
//! `trash-origins` under the data directory, one line per message:
//! summary message ID (hex), account UID and folder full name, separated
//! by tabs. The server keeps no such record, so messages trashed by other
//! clients have none.

use std::path::PathBuf;
use std::sync::Mutex;

/// The list keeps the most recent entries.
const LIMIT: usize = 5000;

#[derive(Clone)]
struct Entry {
    id: u64,
    account: String,
    folder: String,
}

static ENTRIES: Mutex<Option<Vec<Entry>>> = Mutex::new(None);

fn path() -> PathBuf {
    crate::data_dir().join("trash-origins")
}

fn with<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    let mut guard = ENTRIES.lock().unwrap();
    let entries = guard.get_or_insert_with(|| {
        std::fs::read_to_string(path())
            .map(|s| {
                s.lines()
                    .filter_map(|l| {
                        let mut parts = l.splitn(3, '\t');
                        let id = u64::from_str_radix(parts.next()?, 16).ok()?;
                        Some(Entry { id, account: parts.next()?.to_string(), folder: parts.next()?.to_string() })
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    f(entries)
}

fn save(entries: &[Entry]) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = entries.iter().map(|e| format!("{:x}\t{}\t{}\n", e.id, e.account, e.folder)).collect();
    if let Err(e) = std::fs::write(&p, text) {
        glib::g_warning!("salutations", "Cannot save {}: {}", p.display(), e);
    }
}

/// Records where the messages (by summary message ID) were before Trash.
/// An ID of 0 means the message has no Message-ID and is skipped.
pub fn record(account: &str, folder: &str, ids: &[u64]) {
    with(|entries| {
        entries.retain(|e| !ids.contains(&e.id));
        entries.extend(ids.iter().filter(|id| **id != 0).map(|&id| Entry {
            id,
            account: account.to_string(),
            folder: folder.to_string(),
        }));
        if entries.len() > LIMIT {
            entries.drain(..entries.len() - LIMIT);
        }
        save(entries);
    });
}

/// The folder the message came from, if it was trashed in `account` by
/// Salutations.
pub fn lookup(account: &str, id: u64) -> Option<String> {
    with(|entries| entries.iter().rev().find(|e| e.id == id && e.account == account).map(|e| e.folder.clone()))
}

/// Forgets restored messages.
pub fn forget(ids: &[u64]) {
    with(|entries| {
        let before = entries.len();
        entries.retain(|e| !ids.contains(&e.id));
        if entries.len() != before {
            save(entries);
        }
    });
}
