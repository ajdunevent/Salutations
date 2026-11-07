use std::path::Path;

use glib::translate::*;

/// Initializes Camel. `certdb_dir` holds the NSS certificate database.
/// Must be called once before any other Camel function.
pub fn init(certdb_dir: impl AsRef<Path>, nss_init: bool) -> Result<(), glib::BoolError> {
    let ret = unsafe { ffi::camel_init(certdb_dir.as_ref().to_glib_none().0, nss_init.into_glib()) };
    if ret == 0 {
        Ok(())
    } else {
        Err(glib::bool_error!("camel_init() failed with {ret}"))
    }
}

pub fn shutdown() {
    unsafe { ffi::camel_shutdown() }
}

/// Loads every Camel provider module and returns their protocols.
#[doc(alias = "camel_provider_list")]
pub fn load_providers() -> Vec<glib::GString> {
    unsafe {
        let list = ffi::camel_provider_list(glib::ffi::GTRUE);
        let mut protocols = Vec::new();
        let mut node = list;
        while !node.is_null() {
            let provider = (*node).data as *const ffi::CamelProvider;
            if !provider.is_null() && !(*provider).protocol.is_null() {
                protocols.push(from_glib_none((*provider).protocol));
            }
            node = (*node).next;
        }
        glib::ffi::g_list_free(list);
        protocols
    }
}

/// A new Message-ID value, without angle brackets, ending in `domain`.
#[doc(alias = "camel_header_msgid_generate")]
pub fn header_msgid_generate(domain: Option<&str>) -> glib::GString {
    unsafe { from_glib_full(ffi::camel_header_msgid_generate(domain.to_glib_none().0)) }
}

/// Sets or clears an IMAP keyword (a Camel user flag) on a message.
/// Returns whether it changed.
#[doc(alias = "camel_message_info_set_user_flag")]
pub fn message_info_set_user_flag(info: &impl glib::prelude::IsA<crate::MessageInfo>, name: &str, state: bool) -> bool {
    unsafe {
        from_glib(ffi::camel_message_info_set_user_flag(
            info.as_ref().to_glib_none().0,
            name.to_glib_none().0,
            state.into_glib(),
        ))
    }
}

/// Whether a message has an IMAP keyword (a Camel user flag).
#[doc(alias = "camel_message_info_get_user_flag")]
pub fn message_info_user_flag(info: &impl glib::prelude::IsA<crate::MessageInfo>, name: &str) -> bool {
    unsafe { from_glib(ffi::camel_message_info_get_user_flag(info.as_ref().to_glib_none().0, name.to_glib_none().0)) }
}
