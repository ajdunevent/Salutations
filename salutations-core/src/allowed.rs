//! Messages whose remote content the user allowed with "For this message",
//! by Message-ID, in `allowed-messages` under the data directory; and how
//! far the user loosened tracking removal for a message, in
//! `loosened-messages`.

use std::path::PathBuf;
use std::sync::Mutex;

/// The list keeps the most recently used IDs.
const LIMIT: usize = 5000;

static IDS: Mutex<Option<Vec<String>>> = Mutex::new(None);

fn path() -> PathBuf {
    crate::data_dir().join("allowed-messages")
}

fn with<R>(f: impl FnOnce(&mut Vec<String>) -> R) -> R {
    let mut guard = IDS.lock().unwrap();
    let ids = guard.get_or_insert_with(|| {
        std::fs::read_to_string(path())
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    });
    f(ids)
}

fn save(ids: &[String]) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut text = ids.join("\n");
    text.push('\n');
    if let Err(e) = std::fs::write(&p, text) {
        glib::g_warning!("salutations", "Cannot save {}: {}", p.display(), e);
    }
}

/// Whether remote content is allowed for `message_id`. A hit moves the ID
/// to the recent end, in memory only; the order is written on the next add.
pub fn contains(message_id: &str) -> bool {
    with(|ids| match ids.iter().position(|i| i == message_id) {
        Some(n) => {
            let id = ids.remove(n);
            ids.push(id);
            true
        }
        None => false,
    })
}

pub fn add(message_id: &str) {
    with(|ids| {
        ids.retain(|i| i != message_id);
        ids.push(message_id.to_string());
        if ids.len() > LIMIT {
            ids.drain(..ids.len() - LIMIT);
        }
        save(ids);
    });
}

pub fn remove(message_id: &str) {
    with(|ids| {
        let before = ids.len();
        ids.retain(|i| i != message_id);
        if ids.len() != before {
            save(ids);
        }
    });
}

pub fn clear() {
    with(|ids| {
        ids.clear();
        let _ = std::fs::remove_file(path());
    });
}

static LOOSENED: Mutex<Option<Vec<(String, crate::remote::Strip)>>> = Mutex::new(None);

fn loosened_path() -> PathBuf {
    crate::data_dir().join("loosened-messages")
}

fn with_loosened<R>(f: impl FnOnce(&mut Vec<(String, crate::remote::Strip)>) -> R) -> R {
    use crate::remote::Strip;
    let mut guard = LOOSENED.lock().unwrap();
    let list = guard.get_or_insert_with(|| {
        std::fs::read_to_string(loosened_path())
            .map(|s| {
                s.lines()
                    .filter_map(|l| {
                        let (level, id) = l.split_once(' ')?;
                        let level = match level {
                            "address" => Strip::Address,
                            "nothing" => Strip::Nothing,
                            _ => return None,
                        };
                        Some((id.to_string(), level))
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    f(list)
}

/// How much is still removed from `message_id`'s remote addresses: All
/// unless the user loosened it.
pub fn strip_level(message_id: &str) -> crate::remote::Strip {
    with_loosened(|list| list.iter().find(|(id, _)| id == message_id).map(|e| e.1)).unwrap_or(crate::remote::Strip::All)
}

pub fn set_strip_level(message_id: &str, level: crate::remote::Strip) {
    use crate::remote::Strip;
    with_loosened(|list| {
        list.retain(|(id, _)| id != message_id);
        if level != Strip::All {
            list.push((message_id.to_string(), level));
        }
        if list.len() > LIMIT {
            list.drain(..list.len() - LIMIT);
        }
        let mut text = String::new();
        for (id, level) in list.iter() {
            text.push_str(if *level == Strip::Address { "address " } else { "nothing " });
            text.push_str(id);
            text.push('\n');
        }
        let p = loosened_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, text) {
            glib::g_warning!("salutations", "Cannot save {}: {}", p.display(), e);
        }
    });
}
