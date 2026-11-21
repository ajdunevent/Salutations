//! Timing lines on stderr when `SALUTATIONS_DEBUG` is set, for finding
//! where startup and sync spend their time.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Option<Instant>> = OnceLock::new();

/// Call first thing in main() so times count from process start.
pub fn init() {
    START.get_or_init(|| std::env::var_os("SALUTATIONS_DEBUG").map(|_| Instant::now()));
}

pub fn log(msg: &str) {
    if let Some(Some(start)) = START.get() {
        eprintln!("[{:7.3}] {msg}", start.elapsed().as_secs_f64());
    }
}
