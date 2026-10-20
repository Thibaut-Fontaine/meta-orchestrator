//! Persistent orchestration state (`state.json`) and cached plan.
//!
//! Writes are atomic (temp file + rename) so an interrupted run never leaves a
//! corrupt state file.

use crate::model::{Plan, ProjectState, ProjectStatus};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectState>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for State {
    fn default() -> Self {
        State {
            version: STATE_VERSION,
            updated_at: crate::util::now_rfc3339(),
            plan: None,
            projects: BTreeMap::new(),
        }
    }
}

impl State {
    /// Insert or replace one project, preserving its creation time.
    pub fn upsert(&mut self, mut project: ProjectState) {
        if let Some(existing) = self.projects.get(&project.slug) {
            if project.created_at.is_empty() {
                project.created_at = existing.created_at.clone();
            }
        }
        if project.created_at.is_empty() {
            project.created_at = crate::util::now_rfc3339();
        }
        project.updated_at = crate::util::now_rfc3339();
        self.projects.insert(project.slug.clone(), project);
    }

    pub fn update_status(&mut self, slug: &str, status: ProjectStatus) {
        if let Some(project) = self.projects.get_mut(slug) {
            project.status = status;
            project.updated_at = crate::util::now_rfc3339();
            if status != ProjectStatus::Running {
                project.pid = None;
            }
        }
    }

    pub fn project(&self, slug: &str) -> Option<&ProjectState> {
        self.projects.get(slug)
    }

    /// Projects in stable (alphabetical) order.
    pub fn ordered(&self) -> Vec<&ProjectState> {
        self.projects.values().collect()
    }
}

/// Load state; a missing file yields an empty state, a corrupt file is an error
/// (never silently discarded).
pub fn load(path: &Path) -> Result<State> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let state: State = serde_json::from_str(&text)
                .with_context(|| format!("parsing state file {}", path.display()))?;
            Ok(state)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(err) => Err(err).with_context(|| format!("reading state file {}", path.display())),
    }
}

/// Atomically write state.
pub fn save(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    let mut to_write = state.clone();
    to_write.version = STATE_VERSION;
    to_write.updated_at = crate::util::now_rfc3339();
    let json = serde_json::to_string_pretty(&to_write)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    crate::util::write_private(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Load the cached plan. A missing file is not an error.
pub fn load_plan(path: &Path) -> Result<Option<Plan>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            Ok(Some(serde_json::from_str(&text).with_context(|| {
                format!("parsing plan file {}", path.display())
            })?))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading plan {}", path.display())),
    }
}

pub fn save_plan(path: &Path, plan: &Plan) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(plan)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    crate::util::write_private(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = load(&dir.path().join("state.json")).unwrap();
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.projects.is_empty());
    }

    #[test]
    fn roundtrip_and_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::default();
        let mut project = ProjectState::new("demo", "Demo", "/tmp/demo");
        state.upsert(project.clone());
        project.status = ProjectStatus::Running;
        project.cores = vec![1, 2];
        state.upsert(project);
        save(&path, &state).unwrap();

        let loaded = load(&path).unwrap();
        let project = loaded.project("demo").unwrap();
        assert_eq!(project.status, ProjectStatus::Running);
        assert_eq!(project.cores, vec![1, 2]);
        assert_eq!(
            project.created_at,
            loaded.project("demo").unwrap().created_at
        );
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save(&path, &State::default()).unwrap();
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["state.json"]);
    }

    #[test]
    fn corrupt_state_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn status_updates_clear_pid() {
        let mut state = State::default();
        let mut project = ProjectState::new("demo", "Demo", "/tmp/demo");
        project.pid = Some(42);
        project.status = ProjectStatus::Running;
        state.upsert(project);
        state.update_status("demo", ProjectStatus::Completed);
        let project = state.project("demo").unwrap();
        assert_eq!(project.status, ProjectStatus::Completed);
        assert_eq!(project.pid, None);
    }

    #[test]
    fn plan_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.json");
        assert!(load_plan(&path).unwrap().is_none());
        let plan = Plan {
            created_at: "now".into(),
            source: "ideas.txt".into(),
            host: None,
            specs: vec![],
        };
        save_plan(&path, &plan).unwrap();
        assert_eq!(load_plan(&path).unwrap().unwrap(), plan);
    }
}
