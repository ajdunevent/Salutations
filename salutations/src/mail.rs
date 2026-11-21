//! Camel as the window uses it: its own MIME and OpenPGP types, and the
//! background process's folders, stores, and summaries in place of Camel's.

pub mod camel {
    pub use crate::remote::{
        Folder, MessageInfo, Session, Store, message_info_set_user_flag, message_info_user_flag, thread_folder,
    };
    pub use salutations_core::camel::*;
}
