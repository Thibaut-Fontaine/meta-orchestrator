//! Exclusive locks for mutating commands.
//!
//! Two `mox up` runs (or an `up` and a `clean --apply`) must never overlap. The
//! lock is a pid file created with `O_EXCL`; a lock left by a dead process is
//! reclaimed automatically. This avoids a `libc`/`flock` dependency while
//! remaining safe against the failure mode that matters here: a crashed run.

use anyhow::{bail, Context, Result};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// An acquired lock, released on drop.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    pid: u32,
}

impl LockGuard {
    /// Acquire `path`, reclaiming it if its owner is dead.
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::util::ensure_dir_private(parent)?;
        }
        let pid = std::process::id();
        match Self::create(path, pid) {
            Ok(guard) => Ok(guard),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let owner = Self::read_pid(path).unwrap_or(0);
                if owner != 0 && is_pid_alive(owner) && process_is_mox(owner) {
                    bail!(
                        "another mox is running (pid {owner}, lock {}). \
                         Wait for it to finish or stop it with `mox stop`.",
                        path.display()
                    );
                }
                // Stale lock: reclaim it once.
                let _ = std::fs::remove_file(path);
                Self::create(path, pid).with_context(|| {
                    format!(
                        "acquiring lock {} after reclaiming a stale one",
                        path.display()
                    )
                })
            }
            Err(err) => Err(err).with_context(|| format!("creating lock {}", path.display())),
        }
    }

    fn create(path: &Path, pid: u32) -> std::io::Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        writeln!(file, "{pid}")?;
        drop(file);
        let _ = crate::util::set_mode(path, 0o600);
        Ok(LockGuard {
            path: path.to_path_buf(),
            pid,
        })
    }

    fn read_pid(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if Self::read_pid(&self.path) == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// True when the pid exists (Linux `/proc`).
pub fn is_pid_alive(pid: u32) -> bool {
    pid != 0 && Path::new("/proc").join(pid.to_string()).exists()
}

/// True when the pid's command line looks like this orchestrator. Used to avoid
/// treating a recycled pid as a live lock owner.
pub fn process_is_mox(pid: u32) -> bool {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&bytes).replace('\0', " ");
    cmdline.contains("mox") || cmdline.contains("meta-orchestrator")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("up.lock");
        let guard = LockGuard::acquire(&path).unwrap();
        assert!(path.exists());
        assert_eq!(
            LockGuard::read_pid_for_test(&path),
            Some(std::process::id())
        );
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn second_acquisition_fails_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("up.lock");
        let _guard = LockGuard::acquire(&path).unwrap();
        let err = LockGuard::acquire(&path).unwrap_err().to_string();
        assert!(err.contains("another mox is running"));
    }

    #[test]
    fn reclaims_a_dead_owners_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("up.lock");
        // A pid that cannot exist.
        std::fs::write(&path, "99999999\n").unwrap();
        let guard = LockGuard::acquire(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&guard.path).unwrap().trim(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn keeps_a_living_mox_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("up.lock");
        // Our own pid looks like a live mox process in this test binary.
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        let err = LockGuard::acquire(&path).unwrap_err().to_string();
        assert!(err.contains("another mox is running"));
    }

    #[test]
    fn lock_is_private() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("x.lock");
            let _guard = LockGuard::acquire(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn pid_helpers() {
        assert!(is_pid_alive(std::process::id()));
        assert!(!is_pid_alive(99999999));
        assert!(!process_is_mox(99999999));
    }

    impl LockGuard {
        fn read_pid_for_test(path: &Path) -> Option<u32> {
            Self::read_pid(path)
        }
    }
}
