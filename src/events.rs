//! Append-only local event log (`events.jsonl`) for observability.
//!
//! Events are small JSON lines written with `O_APPEND` and 0600 permissions.
//! A failed write must never fail the command that produced it.

use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub at: DateTime<Utc>,
    pub kind: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub detail: String,
}

pub fn path(cfg: &Config) -> PathBuf {
    cfg.state_dir_path().join("events.jsonl")
}

/// Append one event. Best-effort: callers usually ignore the error.
pub fn record(cfg: &Config, kind: &str, slug: &str, detail: &str) -> Result<()> {
    crate::util::ensure_dir_private(&cfg.state_dir_path())?;
    let event = EventRecord {
        at: Utc::now(),
        kind: kind.to_string(),
        slug: slug.to_string(),
        detail: detail.to_string(),
    };
    let file_path = path(cfg);
    let mut file = crate::util::open_private_append(&file_path)?;
    serde_json::to_writer(&mut file, &event)?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Last `n` raw JSONL lines.
pub fn tail(cfg: &Config, n: usize) -> Vec<String> {
    crate::util::tail_lines(&path(cfg), n).unwrap_or_default()
}

/// Parsed last `n` events, ignoring malformed lines.
pub fn tail_parsed(cfg: &Config, n: usize) -> Vec<EventRecord> {
    tail(cfg, n)
        .into_iter()
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &std::path::Path) -> Config {
        Config {
            state_dir: root.display().to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn records_and_tails_events() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        record(&cfg, "plan", "", "2 projects").unwrap();
        record(&cfg, "run", "demo", "completed").unwrap();
        let events = tail_parsed(&cfg, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "plan");
        assert_eq!(events[1].slug, "demo");
        assert!(events[1].at <= Utc::now());
    }

    #[test]
    fn tail_limits_and_handles_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        assert!(tail(&cfg, 5).is_empty());
        for i in 0..5 {
            record(&cfg, "x", "", &i.to_string()).unwrap();
        }
        assert_eq!(tail(&cfg, 2).len(), 2);
    }
}
