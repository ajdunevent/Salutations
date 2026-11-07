use std::cell::{Cell, OnceCell, RefCell};

use gtk::{gio, glib, prelude::*, subclass::prelude::*};

/// Sync state shown by an account row's button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[enum_type(name = "SalutationsSyncState")]
pub enum SyncState {
    #[default]
    Idle,
    Syncing,
    Offline,
    Error,
}

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::FolderObject)]
    pub struct FolderObject {
        #[property(get, set)]
        pub title: RefCell<String>,
        #[property(get, set)]
        pub icon_name: RefCell<String>,
        /// -1 while the server has not reported a count.
        #[property(get, set, default = -1)]
        pub unread: Cell<i32>,
        #[property(get, set)]
        pub is_account: Cell<bool>,
        #[property(get, set)]
        pub account_uid: RefCell<String>,
        /// Empty for account rows.
        #[property(get, set)]
        pub full_name: RefCell<String>,
        #[property(get, set, builder(SyncState::Idle))]
        pub sync_state: Cell<SyncState>,
        #[property(get, set)]
        pub sync_message: RefCell<String>,
        pub children: OnceCell<gio::ListStore>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FolderObject {
        const NAME: &'static str = "SalutationsFolderObject";
        type Type = super::FolderObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for FolderObject {}
}

glib::wrapper! {
    /// One row of the folder pane: an account or a folder.
    pub struct FolderObject(ObjectSubclass<imp::FolderObject>);
}

impl FolderObject {
    pub fn account(uid: &str, title: &str) -> Self {
        glib::Object::builder()
            .property("title", title)
            .property("is-account", true)
            .property("account-uid", uid)
            .build()
    }

    pub fn folder(account_uid: &str, node: &salutations_core::camel::FolderNode) -> Self {
        let obj: Self = glib::Object::builder()
            .property("title", folder_title(node))
            .property("icon-name", folder_icon(node.flags))
            .property("unread", node.unread)
            .property("account-uid", account_uid)
            .property("full-name", node.full_name.as_str())
            .build();
        for child in &node.children {
            obj.children().append(&FolderObject::folder(account_uid, child));
        }
        obj
    }

    pub fn children(&self) -> &gio::ListStore {
        self.imp()
            .children
            .get_or_init(gio::ListStore::new::<FolderObject>)
    }

    /// Depth-first search by full name.
    pub fn find(&self, full_name: &str) -> Option<FolderObject> {
        let store = self.children();
        for i in 0..store.n_items() {
            let child = store.item(i).and_downcast::<FolderObject>()?;
            if child.full_name() == full_name {
                return Some(child);
            }
            if let Some(found) = child.find(full_name) {
                return Some(found);
            }
        }
        None
    }
}

fn folder_title(node: &salutations_core::camel::FolderNode) -> String {
    if folder_type(node.flags) == salutations_core::camel::FolderInfoFlags::TYPE_INBOX.bits()
        || node.full_name.eq_ignore_ascii_case("INBOX")
    {
        return "Inbox".into();
    }
    node.display_name.clone()
}

fn folder_icon(flags: salutations_core::camel::FolderInfoFlags) -> &'static str {
    use salutations_core::camel::FolderInfoFlags as F;
    match folder_type(flags) {
        t if t == F::TYPE_INBOX.bits() => "salutations-inbox-symbolic",
        t if t == F::TYPE_SENT.bits() => "mail-send-symbolic",
        t if t == F::TYPE_DRAFTS.bits() => "document-edit-symbolic",
        t if t == F::TYPE_TRASH.bits() => "user-trash-symbolic",
        t if t == F::TYPE_JUNK.bits() => "mail-mark-junk-symbolic",
        t if t == F::TYPE_ARCHIVE.bits() => "salutations-archive-symbolic",
        _ => "folder-symbolic",
    }
}

/// The folder type is a 6-bit value at bit 10 (`CAMEL_FOLDER_TYPE_MASK`);
/// TRASH is 3 << 10 and contains INBOX's bit.
fn folder_type(flags: salutations_core::camel::FolderInfoFlags) -> u32 {
    flags.bits() & (0x3f << 10)
}
