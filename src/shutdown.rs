//! Ctrl-C / SIGTERM handling shared by the runner.
//!
//! A signal handler only flips an atomic flag; the runner polls it, kills the
//! agent's whole process group and returns a normal "cancelled" outcome so the
//! app can save state and print a summary. This avoids orphaning agent
//! processes.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);
static REASON: AtomicU8 = AtomicU8::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    None,
    User,
    Disk,
}

pub fn reason() -> StopReason {
    match REASON.load(Ordering::SeqCst) {
        1 => StopReason::User,
        2 => StopReason::Disk,
        _ => StopReason::None,
    }
}

pub fn request_disk() {
    REASON.store(2, Ordering::SeqCst);
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Reset state before a new command.
pub fn clear() {
    REQUESTED.store(false, Ordering::SeqCst);
    REASON.store(0, Ordering::SeqCst);
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Test/helper hook; never used by production code.
pub fn request() {
    REASON.store(1, Ordering::SeqCst);
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
    fn flag_roundtrip_and_reason() {
        clear();
        assert!(!requested());
        assert_eq!(reason(), StopReason::None);
        request();
        assert!(requested());
        assert_eq!(reason(), StopReason::User);
        request_disk();
        assert_eq!(reason(), StopReason::Disk);
        clear();
    }
}
