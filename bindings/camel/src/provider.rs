use glib::translate::*;

use crate::Provider;

impl Provider {
    /// Protocol name, such as "imapx" or "smtp".
    pub fn protocol(&self) -> Option<glib::GString> {
        unsafe { from_glib_none((*self.as_ptr()).protocol) }
    }

    pub fn name(&self) -> Option<glib::GString> {
        unsafe { from_glib_none((*self.as_ptr()).name) }
    }
}
