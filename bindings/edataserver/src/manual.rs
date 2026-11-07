use glib::translate::*;

use crate::{Source, SourceExtension, ffi, prelude::*};

macro_rules! str_constants {
    ($($name:ident),* $(,)?) => {
        $(
            pub const $name: &str = match std::ffi::CStr::from_bytes_with_nul(ffi::$name) {
                Ok(s) => match s.to_str() {
                    Ok(s) => s,
                    Err(_) => panic!(),
                },
                Err(_) => panic!(),
            };
        )*
    };
}

str_constants!(
    E_SOURCE_EXTENSION_AUTHENTICATION,
    E_SOURCE_EXTENSION_COLLECTION,
    E_SOURCE_EXTENSION_GOA,
    E_SOURCE_EXTENSION_MAIL_ACCOUNT,
    E_SOURCE_EXTENSION_MAIL_IDENTITY,
    E_SOURCE_EXTENSION_MAIL_SUBMISSION,
    E_SOURCE_EXTENSION_MAIL_TRANSPORT,
    E_SOURCE_EXTENSION_OFFLINE,
    E_SOURCE_EXTENSION_REFRESH,
    E_SOURCE_CREDENTIAL_PASSWORD,
);

pub trait SourceExtManual: IsA<Source> + 'static {
    /// Returns the extension named `name` if the source has one.
    /// `e_source_get_extension()` creates missing extensions, so the
    /// presence check comes first.
    #[doc(alias = "e_source_get_extension")]
    fn extension<T: IsA<SourceExtension>>(&self, name: &str) -> Option<T> {
        if !self.has_extension(name) {
            return None;
        }
        unsafe {
            let ext: Option<SourceExtension> = from_glib_none(ffi::e_source_get_extension(
                self.as_ref().to_glib_none().0,
                name.to_glib_none().0,
            ));
            ext.and_then(|e| e.downcast().ok())
        }
    }
}

impl<O: IsA<Source>> SourceExtManual for O {}

#[doc(alias = "e_util_can_use_collection_as_credential_source")]
pub fn util_can_use_collection_as_credential_source(
    collection_source: Option<&impl IsA<Source>>,
    child_source: &impl IsA<Source>,
) -> bool {
    unsafe {
        from_glib(ffi::e_util_can_use_collection_as_credential_source(
            collection_source.map(|s| s.as_ref()).to_glib_none().0,
            child_source.as_ref().to_glib_none().0,
        ))
    }
}
