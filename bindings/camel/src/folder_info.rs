use glib::translate::*;

use crate::{FolderInfo, FolderInfoFlags, ffi};

/// Owned snapshot of one `CamelFolderInfo` node and its subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderNode {
    pub full_name: String,
    pub display_name: String,
    pub flags: FolderInfoFlags,
    /// -1 when the store does not know the count.
    pub unread: i32,
    /// -1 when the store does not know the count.
    pub total: i32,
    pub children: Vec<FolderNode>,
}

impl FolderInfo {
    /// Copies this node, its siblings and all their children.
    pub fn to_nodes(&self) -> Vec<FolderNode> {
        unsafe { nodes(self.as_ptr()) }
    }
}

unsafe fn nodes(mut fi: *const ffi::CamelFolderInfo) -> Vec<FolderNode> {
    let mut out = Vec::new();
    while !fi.is_null() {
        unsafe {
            let full_name: Option<String> = from_glib_none((*fi).full_name);
            let display_name: Option<String> = from_glib_none((*fi).display_name);
            out.push(FolderNode {
                full_name: full_name.unwrap_or_default(),
                display_name: display_name.unwrap_or_default(),
                flags: from_glib((*fi).flags),
                unread: (*fi).unread,
                total: (*fi).total,
                children: nodes((*fi).child),
            });
            fi = (*fi).next;
        }
    }
    out
}
