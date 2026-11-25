use std::collections::HashMap;
use std::ptr;

use glib::translate::*;

use crate::{Store, ffi, prelude::*};

pub trait StoreExtManual: IsA<Store> + 'static {
    /// The account settings Camel suggests from the server, such as which
    /// folders are Sent and Drafts. Keys are the `CAMEL_STORE_SETUP_*`
    /// strings; folder values of type `f` are `folder://` URIs.
    #[doc(alias = "camel_store_initial_setup_sync")]
    fn initial_setup_sync(
        &self,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<HashMap<String, String>, glib::Error> {
        unsafe {
            let mut table = ptr::null_mut();
            let mut error = ptr::null_mut();
            ffi::camel_store_initial_setup_sync(
                self.as_ref().to_glib_none().0,
                &mut table,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
                &mut error,
            );
            if !error.is_null() {
                if !table.is_null() {
                    glib::ffi::g_hash_table_unref(table);
                }
                return Err(from_glib_full(error));
            }
            let mut out = HashMap::new();
            if table.is_null() {
                return Ok(out);
            }
            let mut iter = std::mem::MaybeUninit::<glib::ffi::GHashTableIter>::uninit();
            glib::ffi::g_hash_table_iter_init(iter.as_mut_ptr(), table);
            let mut key = ptr::null_mut();
            let mut value = ptr::null_mut();
            while glib::ffi::g_hash_table_iter_next(iter.as_mut_ptr(), &mut key, &mut value) != 0 {
                if key.is_null() || value.is_null() {
                    continue;
                }
                out.insert(
                    glib::GStr::from_ptr(key as *const _).to_string(),
                    glib::GStr::from_ptr(value as *const _).to_string(),
                );
            }
            glib::ffi::g_hash_table_unref(table);
            Ok(out)
        }
    }

    /// Creates `name` under `parent` (None for the top level). Blocks.
    #[doc(alias = "camel_store_create_folder_sync")]
    fn create_folder_sync(
        &self,
        parent: Option<&str>,
        name: &str,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<(), glib::Error> {
        unsafe {
            let mut error = ptr::null_mut();
            let info = ffi::camel_store_create_folder_sync(
                self.as_ref().to_glib_none().0,
                parent.to_glib_none().0,
                name.to_glib_none().0,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
                &mut error,
            );
            if !info.is_null() {
                ffi::camel_folder_info_free(info);
            }
            if !error.is_null() {
                return Err(from_glib_full(error));
            }
            Ok(())
        }
    }

    /// Whether the store records a subscription to `name`, without asking
    /// the server. False for a store without subscriptions.
    #[doc(alias = "camel_subscribable_folder_is_subscribed")]
    fn is_subscribed(&self, name: &str) -> bool {
        unsafe {
            let store: *mut ffi::CamelStore = self.as_ref().to_glib_none().0;
            glib::gobject_ffi::g_type_check_instance_is_a(store as *mut _, ffi::camel_subscribable_get_type()) != 0
                && ffi::camel_subscribable_folder_is_subscribed(store as *mut ffi::CamelSubscribable, name.to_glib_none().0) != 0
        }
    }

    /// Subscribes to `name` on the server, or unsubscribes. Blocks. A store
    /// without subscriptions does nothing.
    #[doc(alias = "camel_subscribable_subscribe_folder_sync")]
    #[doc(alias = "camel_subscribable_unsubscribe_folder_sync")]
    fn set_subscribed_sync(
        &self,
        name: &str,
        subscribed: bool,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<(), glib::Error> {
        unsafe {
            let store: *mut ffi::CamelStore = self.as_ref().to_glib_none().0;
            let is = glib::gobject_ffi::g_type_check_instance_is_a(store as *mut _, ffi::camel_subscribable_get_type());
            if is == 0 {
                return Ok(());
            }
            let f = if subscribed {
                ffi::camel_subscribable_subscribe_folder_sync
            } else {
                ffi::camel_subscribable_unsubscribe_folder_sync
            };
            let mut error = ptr::null_mut();
            f(
                store as *mut ffi::CamelSubscribable,
                name.to_glib_none().0,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
                &mut error,
            );
            if !error.is_null() {
                return Err(from_glib_full(error));
            }
            Ok(())
        }
    }
}

impl<O: IsA<Store>> StoreExtManual for O {}

pub trait OfflineStoreExtManual: IsA<crate::OfflineStore> + 'static {
    /// Folders set to download messages for offline use. IMAPX returns
    /// NULL rather than an empty array when there are none.
    #[doc(alias = "camel_offline_store_dup_downsync_folders")]
    fn dup_downsync_folders(&self) -> Vec<crate::Folder> {
        unsafe {
            let array = ffi::camel_offline_store_dup_downsync_folders(self.as_ref().to_glib_none().0);
            if array.is_null() {
                return Vec::new();
            }
            FromGlibPtrContainer::from_glib_full(array)
        }
    }
}

impl<O: IsA<crate::OfflineStore>> OfflineStoreExtManual for O {}
