//! Disk reclamation: report and remove known cache/build directories.

use crate::config::Config;
use crate::resources::{
    dir_size_bytes, find_prunable, project_dirs, remove_prunable, Prunable, PruneKind,
};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InternalEntry {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanReport {
    pub project_entries: Vec<Prunable>,
    /// Entries under `.mox/cache`, `.mox/tmp` (safe to delete wholesale).
    pub internal_entries: Vec<InternalEntry>,
    pub project_bytes: u64,
    pub internal_bytes: u64,
}

impl CleanReport {
    pub fn total_bytes(&self) -> u64 {
        self.project_bytes + self.internal_bytes
    }

    pub fn safe_bytes(&self) -> u64 {
        self.project_entries
            .iter()
            .filter(|e| e.kind == PruneKind::Safe)
            .map(|e| e.bytes)
            .sum::<u64>()
            + self.internal_bytes
    }
}

/// Scan every project plus the orchestrator's own cache/tmp trees.
pub fn scan(cfg: &Config) -> CleanReport {
    let mut report = CleanReport::default();
    for project in project_dirs(&cfg.projects_root_path()) {
        for entry in find_prunable(&project) {
            report.project_bytes = report.project_bytes.saturating_add(entry.bytes);
            report.project_entries.push(entry);
        }
    }
    for root in [cfg.cache_dir(), cfg.tmp_dir()] {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                let meta = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.file_type().is_symlink() {
                    continue;
                }
                let bytes = if meta.is_dir() {
                    dir_size_bytes(&path)
                } else {
                    meta.len()
                };
                report.internal_bytes = report.internal_bytes.saturating_add(bytes);
                report.internal_entries.push(InternalEntry { path, bytes });
            }
        }
    }
    report
        .project_entries
        .sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    report
        .internal_entries
        .sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    report
}

/// Delete scanner findings inside `root`, refusing symlinks and paths outside.
fn remove_tree_inside(root: &Path, path: &Path) -> Result<()> {
    if !crate::util::is_inside(root, path) {
        bail!(
            "refusing to delete {}: outside {}",
            path.display(),
            root.display()
        );
    }
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!("refusing to delete symlink {}", path.display());
    }
    if meta.is_dir() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Apply a report. `include_build` also removes `target/`, `dist/`, `build/`.
/// Returns the paths actually removed.
pub fn apply(cfg: &Config, report: &CleanReport, include_build: bool) -> Result<Vec<PathBuf>> {
    let root = cfg.projects_root_path();
    for entry in &report.project_entries {
        if entry.kind == PruneKind::Build && !include_build {
            continue;
        }
        remove_prunable(&root, entry, include_build)?;
    }
    let mut removed: Vec<PathBuf> = report
        .project_entries
        .iter()
        .filter(|e| include_build || e.kind != PruneKind::Build)
        .map(|e| e.path.clone())
        .collect();
    for entry in &report.internal_entries {
        let allowed =
            entry.path.starts_with(cfg.cache_dir()) || entry.path.starts_with(cfg.tmp_dir());
        if !allowed {
            bail!("refusing to delete internal path {}", entry.path.display());
        }
        remove_tree_inside(&cfg.internal_dir(), &entry.path)?;
        removed.push(entry.path.clone());
    }
    Ok(removed)
}

/// Marker files of running agents; `clean --apply` refuses while any exist.
pub fn running_pids(cfg: &Config) -> Vec<(String, u32)> {
    let mut pids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(cfg.run_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(slug) = name.strip_suffix(".pid") else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    pids.push((slug.to_string(), pid));
                }
            }
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &Path) -> Config {
        Config {
            projects_root: root.to_string_lossy().to_string(),
            state_dir: root.join(".state").to_string_lossy().to_string(),
            ..Config::default()
        }
    }

    fn seed(root: &Path) {
        std::fs::create_dir_all(root.join("proj/node_modules/pkg")).unwrap();
        std::fs::write(root.join("proj/node_modules/pkg/i.js"), vec![b'x'; 100]).unwrap();
        std::fs::create_dir_all(root.join("proj/target/debug")).unwrap();
        std::fs::write(root.join("proj/target/debug/bin"), vec![b'x'; 200]).unwrap();
        std::fs::write(root.join("proj/main.rs"), b"fn main() {}").unwrap();
        std::fs::create_dir_all(root.join(".mox/cache/cargo-target/proj")).unwrap();
        std::fs::write(root.join(".mox/cache/cargo-target/proj/x"), vec![b'x'; 50]).unwrap();
        std::fs::create_dir_all(root.join(".mox/tmp/proj")).unwrap();
        std::fs::write(root.join(".mox/tmp/proj/y"), vec![b'x'; 10]).unwrap();
    }

    #[test]
    fn scan_reports_project_and_internal_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root);
        let report = scan(&cfg(root));
        assert_eq!(report.project_entries.len(), 2);
        assert_eq!(report.project_bytes, 300);
        assert_eq!(report.internal_bytes, 60);
        assert_eq!(report.total_bytes(), 360);
        assert_eq!(report.safe_bytes(), 160); // node_modules + internal, no target
    }

    #[test]
    fn apply_skips_build_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root);
        let cfg = cfg(root);
        let report = scan(&cfg);
        let removed = apply(&cfg, &report, false).unwrap();
        assert_eq!(removed.len(), 3); // node_modules + 2 internal entries
        assert!(!root.join("proj/node_modules").exists());
        assert!(root.join("proj/target").exists());
        assert!(!root.join(".mox/cache/cargo-target/proj").exists());
        assert!(root.join("proj/main.rs").exists());
    }

    #[test]
    fn apply_with_build_removes_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root);
        let cfg = cfg(root);
        let report = scan(&cfg);
        apply(&cfg, &report, true).unwrap();
        assert!(!root.join("proj/target").exists());
    }

    #[test]
    fn detects_running_pids() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        std::fs::create_dir_all(cfg.run_dir()).unwrap();
        std::fs::write(cfg.run_dir().join("a.pid"), "999\n").unwrap();
        std::fs::write(cfg.run_dir().join("notes.txt"), "x").unwrap();
        let pids = running_pids(&cfg);
        assert_eq!(pids, vec![("a".to_string(), 999)]);
    }

    #[test]
    fn refuses_internal_path_outside_mox() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let outside = dir.path().join("precious");
        std::fs::create_dir_all(&outside).unwrap();
        let report = CleanReport {
            internal_entries: vec![InternalEntry {
                path: outside,
                bytes: 0,
            }],
            ..CleanReport::default()
        };
        assert!(apply(&cfg, &report, false).is_err());
    }
}
