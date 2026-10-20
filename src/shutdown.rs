//! Ctrl-C / SIGTERM handling shared by the runner.
//!
//! A signal handler only flips an atomic flag; the runner polls it, kills the
//! agent's whole process group and returns a normal "cancelled" outcome so the
//! app can save state and print a summary. This avoids orphaning agent
//! processes.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Test/helper hook; never used by production code.
pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn reset() {
    REQUESTED.store(false, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn handler(_signal: libc::c_int) {
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Install handlers for SIGINT and SIGTERM. Idempotent.
#[cfg(unix)]
pub fn install() {
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
pub fn install() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_roundtrip() {
        reset();
        assert!(!requested());
        request();
        assert!(requested());
        reset();
    }
}
