#![allow(clippy::needless_doctest_main)]

pub use ffi;
pub use gio;
pub use glib;

#[allow(unused_imports, clippy::all)]
mod auto;
pub use auto::*;

mod cipher;
pub use cipher::{GpgContext, KeyInfo, SignStatus, Signer, Validity};

mod folder;
pub use folder::{FolderChanges, FolderExtManual};

mod folder_thread;
pub use folder_thread::{ThreadNode, thread_folder};

mod compat;

mod folder_info;
pub use folder_info::FolderNode;

mod provider;

mod store;
pub use store::{OfflineStoreExtManual, StoreExtManual};

mod sasl;
pub use sasl::{SaslExtManual, sasl_mechanism_needs_password};

mod functions;
pub use functions::*;

pub mod subclass;

pub mod prelude {
    pub use gio::prelude::*;

    pub use crate::{FolderExtManual, OfflineStoreExtManual, SaslExtManual, StoreExtManual};
    pub use crate::auto::traits::*;
}
