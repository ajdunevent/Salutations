pub use camel;
pub use ffi;
pub use gio;
pub use glib;

#[allow(unused_imports, clippy::all)]
mod auto;
pub use auto::*;

pub mod prelude {
    pub use gio::prelude::*;

    pub use crate::SourceExtManual;
    pub use crate::auto::traits::*;
}

mod manual;
pub use manual::*;
