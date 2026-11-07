use std::ptr;

use glib::translate::*;

use crate::{Sasl, ffi, prelude::*};

pub trait SaslExtManual: IsA<Sasl> + 'static {
    /// Whether the mechanism can authenticate without a password from
    /// the caller, for example GSSAPI with an existing Kerberos ticket.
    #[doc(alias = "camel_sasl_try_empty_password_sync")]
    fn try_empty_password_sync(
        &self,
        cancellable: Option<&impl IsA<gio::Cancellable>>,
    ) -> Result<bool, glib::Error> {
        unsafe {
            let mut error = ptr::null_mut();
            let ret = ffi::camel_sasl_try_empty_password_sync(
                self.as_ref().to_glib_none().0,
                cancellable.map(|c| c.as_ref()).to_glib_none().0,
                &mut error,
            );
            if error.is_null() {
                Ok(from_glib(ret))
            } else {
                Err(from_glib_full(error))
            }
        }
    }
}

impl<O: IsA<Sasl>> SaslExtManual for O {}

/// Whether the SASL `mechanism` needs a password. `None` for mechanisms
/// Camel does not know, such as APOP.
#[doc(alias = "camel_sasl_authtype")]
pub fn sasl_mechanism_needs_password(mechanism: &str) -> Option<bool> {
    unsafe {
        let authtype = ffi::camel_sasl_authtype(mechanism.to_glib_none().0);
        (!authtype.is_null()).then(|| from_glib((*authtype).need_password))
    }
}
