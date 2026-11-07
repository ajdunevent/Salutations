mod session;

pub use session::{SessionImpl, SessionImplExt};

pub mod prelude {
    pub use gio::subclass::prelude::*;

    pub use super::{SessionImpl, SessionImplExt};
}
