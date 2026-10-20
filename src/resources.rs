//! Disk-space safety: pre-flight checks, size accounting and safe pruning.
//!
//! The workstation and the VPS both have limited disk. Agents are told to keep
//! caches outside their repository, and this module makes those promises
//! enforceable: work refuses to start below a floor, and `mox clean` can remove
//! only well-known build/cache directories (never arbitrary paths).

use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::exec::{CommandRunner, CommandSpec};

/// Directory names that are safe to delete (pure caches / regenerable).
pub const SAFE_PRUNE_DIRS: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
    ".cache",
    ".gradle",
    "coverage",
    "htmlcov",
    ".terraform",
];

/// Build outputs: regenerable but slower to rebuild. Deleted only with
/// `--all`.
pub const BUILD_PRUNE_DIRS: &[&str] = &["target", "dist", "build", "out", ".output"];

/// How aggressive a prune entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneKind {
    /// Regenerable cache: safe by default.
    Safe,
    /// Build output: only with `--all`.
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prunable {
    pub path: PathBuf,
    pub bytes: u64,
    pub kind: PruneKind,
}

/// Probe available disk space. Injectable so tests never depend on the host.
pub trait DiskProbe: Send + Sync {
    fn free_mb(&self, path: &Path) -> Result<u64>;
}

/// Runs `df -Pm <path>` (POSIX output, 1 MiB blocks).
pub struct DfProbe<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> DfProbe<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        DfProbe { runner }
    }
}

impl DiskProbe for DfProbe<'_> {
    fn free_mb(&self, path: &Path) -> Result<u64> {
        let mut probe_path = path;
        while !probe_path.exists() {
            match probe_path.parent() {
                Some(parent) => probe_path = parent,
                None => break,
            }
        }
        let output = self
            .runner
            .run(
                &CommandSpec::new("df")
                    .args(["-Pm"])
                    .arg(probe_path.display().to_string()),
            )
            .context("running df")?;
        if !output.success() {
            bail!("df failed: {}", output.tail(5));
        }
        parse_df_free_mb(&output.stdout)
            .ok_or_else(|| anyhow::anyhow!("unexpected df output: {}", output.stdout.trim()))
    }
}

/// Parse the `Available` column (4th) from `df -Pm` output.
pub fn parse_df_free_mb(text: &str) -> Option<u64> {
    let line = text.lines().skip(1).find(|l| !l.trim().is_empty())?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return None;
    }
    fields[3].parse().ok()
}

/// Pre-flight policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskGuard {
    pub min_free_mb: u64,
    pub project_budget_mb: u64,
}

impl DiskGuard {
    pub fn new(min_free_mb: u64, project_budget_mb: u64) -> Self {
        DiskGuard {
            min_free_mb: min_free_mb.max(1),
            project_budget_mb: project_budget_mb.max(1),
        }
    }

    /// Refuse to start work when free space is below the floor.
    pub fn ensure_free(&self, free_mb: u64) -> Result<()> {
        if free_mb < self.min_free_mb {
            bail!(
                "not enough free disk: {} available, {} required.\n\
                 Run `mox clean --apply` or free space before launching agents.",
                crate::util::human_mb(free_mb),
                crate::util::human_mb(self.min_free_mb)
            );
        }
        Ok(())
    }

    /// Warn (but do not fail) when a project already exceeds its soft budget.
    pub fn over_budget(&self, project_mb: u64) -> bool {
        project_mb > self.project_budget_mb
    }
}

/// Total size of regular files under `path`, without following symlinks and
/// with a depth limit.
pub fn dir_size_bytes(path: &Path) -> u64 {
    dir_size_bytes_limited(path, 24)
}

fn dir_size_bytes_limited(path: &Path, max_depth: usize) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Find prunable directories for one project, without descending into them.
pub fn find_prunable(project_dir: &Path) -> Vec<Prunable> {
    let mut found = Vec::new();
    let mut stack = vec![(project_dir.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 8 {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let kind = if SAFE_PRUNE_DIRS.contains(&name.as_str()) {
                Some(PruneKind::Safe)
            } else if BUILD_PRUNE_DIRS.contains(&name.as_str()) {
                Some(PruneKind::Build)
            } else {
                None
            };
            if let Some(kind) = kind {
                let path = entry.path();
                found.push(Prunable {
                    bytes: dir_size_bytes(&path),
                    path,
                    kind,
                });
            } else {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    found.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    found
}

/// Delete one prunable entry after verifying it is inside `root`, is not a
/// symlink, and still carries an allowlisted directory name.
pub fn remove_prunable(root: &Path, entry: &Prunable, include_build: bool) -> Result<()> {
    if entry.kind == PruneKind::Build && !include_build {
        bail!(
            "refusing to delete build output {} (pass --all to include it)",
            entry.path.display()
        );
    }
    if !crate::util::is_inside(root, &entry.path) {
        bail!(
            "refusing to delete {}: outside {}",
            entry.path.display(),
            root.display()
        );
    }
    let meta = std::fs::symlink_metadata(&entry.path)
        .with_context(|| format!("stat {}", entry.path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        bail!(
            "refusing to delete {}: not a real directory",
            entry.path.display()
        );
    }
    let name = entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let allowlisted =
        SAFE_PRUNE_DIRS.contains(&name.as_str()) || BUILD_PRUNE_DIRS.contains(&name.as_str());
    if !allowlisted {
        bail!(
            "refusing to delete {}: name not allowlisted",
            entry.path.display()
        );
    }
    std::fs::remove_dir_all(&entry.path)
        .with_context(|| format!("removing {}", entry.path.display()))?;
    Ok(())
}

/// All project directories directly under `root`, ignoring `.mox` and hidden
/// entries.
pub fn project_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                dirs.insert(entry.path());
            }
        }
    }
    dirs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FakeRunner;

    #[test]
    fn parses_df_output() {
        let text = "Filesystem     1048576-blocks      Used Available Capacity Mounted on\n\
                    /dev/nvme0n1p8        189000    176000      9000      95% /\n";
        assert_eq!(parse_df_free_mb(text), Some(9000));
        assert_eq!(parse_df_free_mb("garbage"), None);
    }

    #[test]
    fn guard_refuses_low_disk() {
        let guard = DiskGuard::new(2048, 4096);
        assert!(guard.ensure_free(3000).is_ok());
        let err = guard.ensure_free(100).unwrap_err().to_string();
        assert!(err.contains("not enough free disk"));
        assert!(guard.over_budget(5000));
        assert!(!guard.over_budget(100));
    }

    #[test]
    fn df_probe_uses_runner_and_reports() {
        let runner = FakeRunner::new(|spec| {
            assert_eq!(spec.program, "df");
            assert_eq!(spec.args, vec!["-Pm", "/"]);
            crate::exec::Output {
                code: Some(0),
                stdout: "Filesystem 1048576-blocks Used Available Capacity Mounted on\n/dev/x 100 50 777 50% /\n".into(),
                ..crate::exec::Output::default()
            }
        });
        let probe = DfProbe::new(&runner);
        assert_eq!(probe.free_mb(Path::new("/")).unwrap(), 777);
    }

    #[test]
    fn measures_directory_size_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"abcd").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"abcdef").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", dir.path().join("link")).unwrap();
        assert_eq!(dir_size_bytes(dir.path()), 10);
    }

    #[test]
    fn finds_cache_dirs_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(proj.join("target/debug")).unwrap();
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::write(proj.join("node_modules/pkg/i.js"), b"1234").unwrap();
        std::fs::write(proj.join("target/debug/bin"), b"12345678").unwrap();
        std::fs::write(proj.join("src/main.rs"), b"fn main(){}").unwrap();

        let found = find_prunable(&proj);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, PruneKind::Build); // target is bigger
        assert!(found.iter().any(|p| p.kind == PruneKind::Safe));
        // src must never be reported
        assert!(!found.iter().any(|p| p.path.ends_with("src")));
    }

    #[test]
    fn refuses_build_deletion_without_all() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(proj.join("target")).unwrap();
        let entry = Prunable {
            path: proj.join("target"),
            bytes: 0,
            kind: PruneKind::Build,
        };
        assert!(remove_prunable(dir.path(), &entry, false).is_err());
        assert!(remove_prunable(dir.path(), &entry, true).is_ok());
        assert!(!proj.join("target").exists());
    }

    #[test]
    fn refuses_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("node_modules")).unwrap();
        let entry = Prunable {
            path: outside.path().join("node_modules"),
            bytes: 0,
            kind: PruneKind::Safe,
        };
        let err = remove_prunable(root.path(), &entry, false).unwrap_err();
        assert!(err.to_string().contains("outside"));
    }

    #[test]
    fn refuses_non_allowlisted_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("precious")).unwrap();
        let entry = Prunable {
            path: root.path().join("precious"),
            bytes: 0,
            kind: PruneKind::Safe,
        };
        let err = remove_prunable(root.path(), &entry, false).unwrap_err();
        assert!(err.to_string().contains("not allowlisted"));
    }

    #[test]
    fn lists_project_dirs_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::create_dir(dir.path().join(".mox")).unwrap();
        std::fs::write(dir.path().join("file"), b"x").unwrap();
        let projects = project_dirs(dir.path());
        assert_eq!(projects.len(), 2);
    }
}
