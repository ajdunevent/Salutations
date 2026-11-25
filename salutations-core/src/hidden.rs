//! Hidden folders. A folder is hidden by unsubscribing from it, so other
//! clients that honour subscriptions hide it too. Folders that were
//! unsubscribed when Salutations first listed them, before it hid
//! anything, stay shown: they are kept per account in
//! `folder-visibility.ini` under the data directory, with every folder
//! seen so far, so that one that appears unsubscribed later (made by
//! another client or a server filter) is shown as well.
//!
//! The background process marks the shown folders with
//! `CAMEL_FOLDER_SUBSCRIBED` before sending the tree; the window reads
//! that flag alone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::camel::{FolderInfoFlags, FolderNode};

/// Folders hidden by themselves or with a parent, per account, from the
/// last listing.
static HIDDEN: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

#[cfg(not(test))]
fn path() -> PathBuf {
    crate::data_dir().join("folder-visibility.ini")
}

#[cfg(test)]
fn path() -> PathBuf {
    std::env::temp_dir().join(format!("salutations-hidden-{}.ini", std::process::id()))
}

fn load() -> glib::KeyFile {
    let kf = glib::KeyFile::new();
    let _ = kf.load_from_file(path(), glib::KeyFileFlags::NONE);
    kf
}

fn list(kf: &glib::KeyFile, account: &str, key: &str) -> Vec<String> {
    kf.string_list(account, key).map(|l| l.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
}

fn set_list(kf: &glib::KeyFile, account: &str, key: &str, items: &[String]) {
    use glib::translate::ToGlibPtr;
    // glib-rs has no setter for lists; GLib's escapes ";" in folder names.
    let c: Vec<std::ffi::CString> = items.iter().filter_map(|s| std::ffi::CString::new(s.as_str()).ok()).collect();
    let ptrs: Vec<*const std::ffi::c_char> = c.iter().map(|s| s.as_ptr()).collect();
    unsafe {
        glib::ffi::g_key_file_set_string_list(kf.to_glib_none().0, account.to_glib_none().0, key.to_glib_none().0, ptrs.as_ptr(), ptrs.len());
    }
}

fn save(kf: &glib::KeyFile) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = kf.save_to_file(&p) {
        glib::g_warning!("salutations", "Cannot save {}: {}", p.display(), e);
    }
}

fn is_inbox(path: &str) -> bool {
    path.eq_ignore_ascii_case("INBOX")
}

/// Whether the tree marks `node` as shown in its own right.
pub fn shown(node: &FolderNode) -> bool {
    node.flags.contains(FolderInfoFlags::SUBSCRIBED)
}

/// Marks the folders that are shown. `listed` says the tree comes from the
/// server rather than the cache; only then are new folders recorded.
pub fn mark(account: &str, nodes: &mut [FolderNode], listed: bool) {
    fn paths(nodes: &[FolderNode], out: &mut Vec<String>) {
        for n in nodes {
            out.push(n.full_name.clone());
            paths(&n.children, out);
        }
    }
    let kf = load();
    let mut shown_list = list(&kf, account, "shown");
    if listed && !nodes.is_empty() {
        let known = list(&kf, account, "known");
        let mut all = Vec::new();
        paths(nodes, &mut all);
        fn unsubscribed(nodes: &[FolderNode], out: &mut Vec<String>) {
            for n in nodes {
                if !n.flags.contains(FolderInfoFlags::SUBSCRIBED) {
                    out.push(n.full_name.clone());
                }
                unsubscribed(&n.children, out);
            }
        }
        let mut off = Vec::new();
        unsubscribed(nodes, &mut off);
        let before = shown_list.len();
        shown_list.retain(|p| all.contains(p));
        shown_list.extend(off.into_iter().filter(|p| !known.contains(p) && !is_inbox(p)));
        if known != all || shown_list.len() != before {
            set_list(&kf, account, "known", &all);
            set_list(&kf, account, "shown", &shown_list);
            save(&kf);
        }
    }
    fn walk(nodes: &mut [FolderNode], shown_list: &[String], parent_hidden: bool, hidden: &mut Vec<String>) {
        for n in nodes {
            if is_inbox(&n.full_name) || n.flags.contains(FolderInfoFlags::NOSELECT) || shown_list.contains(&n.full_name) {
                n.flags |= FolderInfoFlags::SUBSCRIBED;
            }
            let off = parent_hidden || !n.flags.contains(FolderInfoFlags::SUBSCRIBED);
            if off {
                hidden.push(n.full_name.clone());
            }
            walk(&mut n.children, shown_list, off, hidden);
        }
    }
    let mut hidden = Vec::new();
    walk(nodes, &shown_list, false, &mut hidden);
    HIDDEN.lock().unwrap().get_or_insert_with(HashMap::new).insert(account.to_string(), hidden);
}

/// Whether `folder` was hidden, by itself or with a parent, at the last
/// listing in this process.
pub fn is_hidden(account: &str, folder: &str) -> bool {
    HIDDEN.lock().unwrap().as_ref().and_then(|m| m.get(account)).is_some_and(|h| h.iter().any(|p| p == folder))
}

/// Stops showing `folder` regardless of its subscription, once it has been
/// unsubscribed to hide it.
pub fn hid(account: &str, folder: &str) {
    let kf = load();
    let mut shown_list = list(&kf, account, "shown");
    let before = shown_list.len();
    shown_list.retain(|p| p != folder);
    if shown_list.len() != before {
        set_list(&kf, account, "shown", &shown_list);
        save(&kf);
    }
}

/// Follows a rename or move of `old` (and its subfolders) to `new`.
pub fn renamed(account: &str, old: &str, new: &str) {
    let kf = load();
    let mut changed = false;
    for key in ["known", "shown"] {
        let items = list(&kf, account, key);
        let moved: Vec<String> = items.iter().map(|p| crate::renamed_path(p, old, new).unwrap_or_else(|| p.clone())).collect();
        if moved != items {
            set_list(&kf, account, key, &moved);
            changed = true;
        }
    }
    if changed {
        save(&kf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, subscribed: bool, children: Vec<FolderNode>) -> FolderNode {
        FolderNode {
            full_name: name.to_string(),
            display_name: name.rsplit('/').next().unwrap_or(name).to_string(),
            flags: if subscribed { FolderInfoFlags::SUBSCRIBED } else { FolderInfoFlags::empty() },
            unread: 0,
            total: 0,
            children,
        }
    }

    #[test]
    fn new_unsubscribed_folders_stay_shown() {
        let tree = || vec![node("INBOX", false, vec![]), node("Lists", false, vec![node("Lists/Rust", true, vec![])]), node("Work", true, vec![])];
        let mut nodes = tree();
        mark("a", &mut nodes, true);
        assert!(nodes.iter().all(shown));
        assert!(!is_hidden("a", "Lists/Rust"));
        // Work is unsubscribed to hide it.
        hid("a", "Work");
        let mut nodes = tree();
        nodes[2].flags = FolderInfoFlags::empty();
        mark("a", &mut nodes, true);
        assert!(!shown(&nodes[2]) && is_hidden("a", "Work"));
        // Lists, shown when first seen, is hidden with its subfolder.
        hid("a", "Lists");
        let mut nodes = tree();
        nodes[2].flags = FolderInfoFlags::empty();
        nodes.push(node("Receipts", false, vec![]));
        mark("a", &mut nodes, true);
        assert!(is_hidden("a", "Lists") && is_hidden("a", "Lists/Rust"));
        assert!(shown(&nodes[3]) && shown(&nodes[0]));
        renamed("a", "Receipts", "Paid");
        let mut nodes = vec![node("Paid", false, vec![])];
        mark("a", &mut nodes, true);
        assert!(shown(&nodes[0]));
        let _ = std::fs::remove_file(path());
    }
}
