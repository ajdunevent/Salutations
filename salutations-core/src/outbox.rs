//! Messages waiting to be sent. Each stays in its account's Drafts folder on
//! the server; the mark that it is waiting is kept on this device only, so
//! another device never sends it a second time.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub account: String,
    /// Its UID in the account's Drafts folder.
    pub uid: String,
    /// Without angle brackets.
    pub message_id: String,
    /// Why the last attempt failed.
    pub error: String,
}

fn path() -> PathBuf {
    crate::data_dir().join("outbox.ini")
}

/// Every waiting message, in the order they were queued.
pub fn load() -> Vec<Waiting> {
    let kf = glib::KeyFile::new();
    if kf.load_from_file(path(), glib::KeyFileFlags::NONE).is_err() {
        return Vec::new();
    }
    kf.groups()
        .iter()
        .filter_map(|g| {
            Some(Waiting {
                message_id: g.to_string(),
                account: kf.string(g, "account").ok()?.to_string(),
                uid: kf.string(g, "uid").ok()?.to_string(),
                error: kf.string(g, "error").map(|e| e.to_string()).unwrap_or_default(),
            })
        })
        .collect()
}

pub fn save(list: &[Waiting]) {
    let kf = glib::KeyFile::new();
    for w in list {
        kf.set_string(&w.message_id, "account", &w.account);
        kf.set_string(&w.message_id, "uid", &w.uid);
        kf.set_string(&w.message_id, "error", &w.error);
    }
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = kf.save_to_file(&p) {
        glib::g_warning!("salutations", "Cannot save the outbox: {}", e);
    }
}
