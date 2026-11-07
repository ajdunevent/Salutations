use glib::{signal::SignalHandlerId, translate::*};

use crate::{Folder, MessageInfo, ffi, prelude::*};

/// UIDs reported by one `CamelFolder::changed` emission.
#[derive(Debug, Clone, Default)]
pub struct FolderChanges {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub recent: Vec<String>,
}

unsafe fn uid_array(array: *mut glib::ffi::GPtrArray) -> Vec<String> {
    unsafe { crate::compat::strings(array) }
}

pub trait FolderExtManual: IsA<Folder> + 'static {
    /// UIDs of all messages in the folder summary.
    fn uids(&self) -> Vec<glib::GString> {
        unsafe { crate::compat::uids(self.as_ref().to_glib_none().0) }
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// UIDs matching a Camel search expression, such as
    /// `(match-all (< (get-sent-date) 1700000000))`. Blocks.
    fn search_sync(
        &self,
        expression: &str,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<Vec<String>, glib::Error> {
        unsafe {
            crate::compat::search(
                self.as_ref().to_glib_none().0,
                expression.to_glib_none().0,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
            )
        }
    }

    /// Writes the folder's persistent properties, such as its offline
    /// sync choice, to its state file.
    fn save_state(&self) {
        unsafe { crate::compat::save_state(self.as_ref().to_glib_none().0) }
    }

    /// The folder's state file. Camel's cache of message bodies is in the
    /// same directory.
    fn state_filename(&self) -> Option<String> {
        unsafe { crate::compat::state_filename(self.as_ref().to_glib_none().0) }
    }

    /// Appends `message` with the message `flags` set. Blocks. Returns the
    /// new UID where the server reported it (IMAP UIDPLUS).
    #[doc(alias = "camel_folder_append_message_sync")]
    fn append_message_sync(
        &self,
        message: &impl IsA<crate::MimeMessage>,
        flags: u32,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<Option<String>, glib::Error> {
        unsafe {
            let info = ffi::camel_message_info_new(std::ptr::null_mut());
            ffi::camel_message_info_set_flags(info, flags, flags);
            let mut uid = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            let ok = ffi::camel_folder_append_message_sync(
                self.as_ref().to_glib_none().0,
                message.as_ref().to_glib_none().0,
                info,
                &mut uid,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
                &mut error,
            );
            glib::gobject_ffi::g_object_unref(info as *mut _);
            if ok == glib::ffi::GFALSE {
                return Err(from_glib_full(error));
            }
            Ok(Option::<glib::GString>::from_glib_full(uid).map(Into::into))
        }
    }

    /// Copies or moves messages to `destination` in the same store. Blocks.
    /// Returns each message's UID in `destination`, in the order of `uids`,
    /// where the server reported it (IMAP UIDPLUS).
    #[doc(alias = "camel_folder_transfer_messages_to_sync")]
    fn transfer_messages_to_sync(
        &self,
        uids: &[String],
        destination: &impl IsA<Folder>,
        delete_originals: bool,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<Vec<Option<String>>, glib::Error> {
        unsafe {
            let array = glib::ffi::g_ptr_array_new_with_free_func(Some(glib::ffi::g_free));
            for uid in uids {
                let p: *mut std::ffi::c_char = uid.as_str().to_glib_full();
                glib::ffi::g_ptr_array_add(array, p as glib::ffi::gpointer);
            }
            let mut transferred = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            let ok = ffi::camel_folder_transfer_messages_to_sync(
                self.as_ref().to_glib_none().0,
                array,
                destination.as_ref().to_glib_none().0,
                delete_originals.into_glib(),
                &mut transferred,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
                &mut error,
            );
            glib::ffi::g_ptr_array_unref(array);
            let moved = crate::compat::take_transferred(transferred);
            if ok == glib::ffi::GFALSE {
                return Err(from_glib_full(error));
            }
            Ok(moved)
        }
    }

    #[doc(alias = "camel_folder_get_message_info")]
    fn message_info(&self, uid: &str) -> Option<MessageInfo> {
        unsafe {
            from_glib_full(ffi::camel_folder_get_message_info(
                self.as_ref().to_glib_none().0,
                uid.to_glib_none().0,
            ))
        }
    }

    /// Camel emits this from its session's main context, after a batch of
    /// summary changes, never once per message.
    #[doc(alias = "changed")]
    fn connect_changed<F: Fn(&Self, &FolderChanges) + Send + Sync + 'static>(
        &self,
        f: F,
    ) -> SignalHandlerId {
        unsafe extern "C" fn trampoline<
            P: IsA<Folder>,
            F: Fn(&P, &FolderChanges) + Send + Sync + 'static,
        >(
            this: *mut ffi::CamelFolder,
            info: *mut ffi::CamelFolderChangeInfo,
            f: glib::ffi::gpointer,
        ) {
            unsafe {
                let f: &F = &*(f as *const F);
                let changes = if info.is_null() {
                    FolderChanges::default()
                } else {
                    FolderChanges {
                        added: uid_array(ffi::camel_folder_change_info_get_added_uids(info)),
                        removed: uid_array(ffi::camel_folder_change_info_get_removed_uids(info)),
                        changed: uid_array(ffi::camel_folder_change_info_get_changed_uids(info)),
                        recent: uid_array(ffi::camel_folder_change_info_get_recent_uids(info)),
                    }
                };
                f(
                    Folder::from_glib_borrow(this).unsafe_cast_ref(),
                    &changes,
                )
            }
        }
        unsafe {
            let f: Box<F> = Box::new(f);
            glib::signal::connect_raw(
                self.as_ptr() as *mut _,
                c"changed".as_ptr() as *const _,
                Some(std::mem::transmute::<*const (), unsafe extern "C" fn()>(
                    trampoline::<Self, F> as *const (),
                )),
                Box::into_raw(f),
            )
        }
    }
}

impl<O: IsA<Folder>> FolderExtManual for O {}
