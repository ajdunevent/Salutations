use glib::translate::*;

use crate::{Folder, MessageInfo, prelude::*};

/// One message of a thread tree, copied out of Camel's thread structure.
#[derive(Debug, Clone)]
pub struct ThreadNode {
    pub info: MessageInfo,
    pub children: Vec<ThreadNode>,
}

/// Threads every message in the folder summary by Message-ID and
/// References. Roots are in summary order. Nodes whose message is missing
/// from the summary (a reply to an unseen message) are dropped and their
/// children promoted.
pub fn thread_folder(folder: &impl IsA<Folder>) -> Vec<ThreadNode> {
    unsafe { crate::compat::thread(folder.as_ref().to_glib_none().0) }
}

impl ThreadNode {
    /// This node and all its descendants, depth first.
    pub fn flatten(&self) -> Vec<MessageInfo> {
        let mut out = vec![self.info.clone()];
        for c in &self.children {
            out.extend(c.flatten());
        }
        out
    }
}
