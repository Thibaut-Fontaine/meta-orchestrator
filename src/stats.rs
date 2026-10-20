//! Cross-project learning: completed milestone timings become a prior for new
//! projects, so the first milestone's ETA is not a blind guess.
//!
//! Records are append-only, written under a lock (several agents can finish a
//! milestone at the same time) and capped to keep the file small. The prior is
//! a median ratio (`actual / planned`) by complexity class, clamped to the same
//! 0.25–4× range as per-project calibration.

use crate::config::Config;
use crate::host::HostProfile;
use crate::lock::LockGuard;
use crate::milestones::Milestone;
use crate::model::{Complexity, ProjectSpec};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const STATS_VERSION: u32 = 1;
pub const MAX_RECORDS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MilestoneRecord {
    pub slug: String,
    pub milestone: String,
    pub title: String,
    pub complexity: Complexity,
    pub estimate_secs: u64,
    pub actual_secs: u64,
    pub finished_at: DateTime<Utc>,
    #[serde(default)]
    pub cpu_ghz: f64,
    #[serde(default)]
    pub logical_cores: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub records: Vec<MilestoneRecord>,
}

fn default_version() -> u32 {
    STATS_VERSION
}

impl Default for StatsStore {
    fn default() -> Self {
        StatsStore {
            version: STATS_VERSION,
            updated_at: crate::util::now_rfc3339(),
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Bucket {
    pub samples: usize,
    pub mean_ratio: f64,
    pub median_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Summary {
    pub samples: usize,
    pub overall: Bucket,
    pub by_complexity: BTreeMap<String, Bucket>,
}

/// Load stats; a missing file is an empty store.
pub fn load(path: &Path) -> Result<StatsStore> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mut store: StatsStore = serde_json::from_str(&text)
                .with_context(|| format!("parsing stats file {}", path.display()))?;
            store.version = STATS_VERSION;
            Ok(store)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StatsStore::default()),
        Err(err) => Err(err).with_context(|| format!("reading stats file {}", path.display())),
    }
}

pub fn load_optional(path: &Path) -> Option<StatsStore> {
    load(path).ok()
}

/// Atomically save stats with private permissions.
pub fn save(path: &Path, store: &StatsStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::util::ensure_dir_private(parent)?;
    }
    let mut to_write = store.clone();
    to_write.version = STATS_VERSION;
    to_write.updated_at = crate::util::now_rfc3339();
    let json = serde_json::to_string_pretty(&to_write)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    crate::util::write_private(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Append one completed milestone. Returns false when the timing is unusable
/// (no estimate or no measured duration).
pub fn record(
    cfg: &Config,
    slug: &str,
    milestone: &Milestone,
    host: &HostProfile,
    now: DateTime<Utc>,
) -> Result<bool> {
    if milestone.estimate_secs == 0 || milestone.actual_secs == 0 {
        return Ok(false);
    }
    crate::util::ensure_dir_private(&cfg.state_dir_path())?;
    let _lock = LockGuard::acquire(&cfg.state_dir_path().join("stats.lock"))?;
    let mut store = load(&cfg.stats_file())?;
    store.records.push(MilestoneRecord {
        slug: slug.to_string(),
        milestone: milestone.id.clone(),
        title: milestone.title.clone(),
        complexity: milestone.complexity,
        estimate_secs: milestone.estimate_secs,
        actual_secs: milestone.actual_secs,
        finished_at: now,
        cpu_ghz: host.cpu_mhz / 1000.0,
        logical_cores: host.cores,
    });
    if store.records.len() > MAX_RECORDS {
        let drop = store.records.len() - MAX_RECORDS;
        store.records.drain(0..drop);
    }
    save(&cfg.stats_file(), &store)?;
    Ok(true)
}

fn bucket(mut ratios: Vec<f64>) -> Bucket {
    if ratios.is_empty() {
        return Bucket::default();
    }
    let samples = ratios.len();
    let mean = ratios.iter().sum::<f64>() / samples as f64;
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if samples % 2 == 1 {
        ratios[samples / 2]
    } else {
        (ratios[samples / 2 - 1] + ratios[samples / 2]) / 2.0
    };
    Bucket {
        samples,
        mean_ratio: mean,
        median_ratio: median,
    }
}

/// Aggregate the raw records into ratios by complexity class.
pub fn summarize(store: &StatsStore) -> Summary {
    let mut all = Vec::new();
    let mut by_complexity: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for record in &store.records {
        if record.estimate_secs == 0 || record.actual_secs == 0 {
            continue;
        }
        let ratio = record.actual_secs as f64 / record.estimate_secs as f64;
        all.push(ratio);
        by_complexity
            .entry(record.complexity.as_str().to_string())
            .or_default()
            .push(ratio);
    }
    Summary {
        samples: all.len(),
        overall: bucket(all),
        by_complexity: by_complexity
            .into_iter()
            .map(|(key, values)| (key, bucket(values)))
            .collect(),
    }
}

/// Prior factor for one complexity class (falls back to the overall median).
pub fn prior_for(summary: &Summary, complexity: Complexity) -> f64 {
    let bucket = summary
        .by_complexity
        .get(complexity.as_str())
        .filter(|b| b.samples > 0)
        .unwrap_or(&summary.overall);
    if bucket.samples == 0 {
        1.0
    } else {
        bucket.median_ratio.clamp(0.25, 4.0)
    }
}

/// Weighted prior for a planned project, based on its milestone mix.
pub fn prior_for_spec(summary: &Summary, spec: &ProjectSpec) -> f64 {
    if summary.samples == 0 {
        return 1.0;
    }
    let planned = if spec.milestones.is_empty() {
        crate::milestones::fallback_for(spec)
    } else {
        spec.milestones.clone()
    };
    let mut weight = 0u32;
    let mut sum = 0.0f64;
    for milestone in &planned {
        let w = milestone.complexity.weight();
        let prior = summary
            .by_complexity
            .get(milestone.complexity.as_str())
            .filter(|b| b.samples > 0)
            .map(|b| b.median_ratio)
            .unwrap_or(summary.overall.median_ratio);
        sum += prior * w as f64;
        weight += w;
    }
    if weight == 0 {
        1.0
    } else {
        (sum / weight as f64).clamp(0.25, 4.0)
    }
}

/// Compact line for the planning prompt.
pub fn prompt_line(summary: &Summary) -> Option<String> {
    if summary.samples == 0 {
        return None;
    }
    let mut line = format!(
        "Historical calibration on this machine: {} completed milestones; overall median {:.2}× planned (mean {:.2}×).",
        summary.samples, summary.overall.median_ratio, summary.overall.mean_ratio
    );
    let mut parts = Vec::new();
    for (complexity, bucket) in &summary.by_complexity {
        parts.push(format!(
            "{} {:.2}× (n={})",
            complexity, bucket.median_ratio, bucket.samples
        ));
    }
    if !parts.is_empty() {
        line.push_str(&format!(" By complexity: {}.", parts.join(", ")));
    }
    line.push_str(
        " Multiply your minute estimates by these factors when they differ materially from 1.0.",
    );
    Some(line)
}

/// Human-readable table for `mox stats`.
pub fn render(summary: &Summary) -> String {
    if summary.samples == 0 {
        return "no completed milestones yet; stats appear after the first `mox milestone done`.\n"
            .to_string();
    }
    let mut out = format!(
        "completed milestones: {}\noverall: median {:.2}× planned, mean {:.2}×\n\n",
        summary.samples, summary.overall.median_ratio, summary.overall.mean_ratio
    );
    out.push_str(&format!(
        "{:<10} {:>8} {:>10} {:>10}\n",
        "complexity", "samples", "median", "mean"
    ));
    for (complexity, bucket) in &summary.by_complexity {
        out.push_str(&format!(
            "{:<10} {:>8} {:>9.2}× {:>9.2}×\n",
            complexity, bucket.samples, bucket.median_ratio, bucket.mean_ratio
        ));
    }
    out.push_str("\nNew projects start with the median ratio for their milestone mix,\n");
    out.push_str("then calibrate on their own completed milestones.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::milestones::MilestoneStatus;
    use crate::model::{Idea, MilestoneSpec};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample(complexity: Complexity, estimate: u64, actual: u64) -> MilestoneRecord {
        MilestoneRecord {
            slug: "demo".into(),
            milestone: "m1".into(),
            title: "x".into(),
            complexity,
            estimate_secs: estimate,
            actual_secs: actual,
            finished_at: t("2026-10-20T12:00:00Z"),
            cpu_ghz: 3.5,
            logical_cores: 8,
        }
    }

    #[test]
    fn summarizes_medians_and_means() {
        let store = StatsStore {
            records: vec![
                sample(Complexity::Low, 100, 200),
                sample(Complexity::Low, 100, 50),
                sample(Complexity::High, 100, 300),
            ],
            ..StatsStore::default()
        };
        let summary = summarize(&store);
        assert_eq!(summary.samples, 3);
        let low = summary.by_complexity.get("low").unwrap();
        assert_eq!(low.samples, 2);
        assert!((low.median_ratio - 1.25).abs() < 0.001);
        assert!((low.mean_ratio - 1.25).abs() < 0.001);
        let high = summary.by_complexity.get("high").unwrap();
        assert!((high.median_ratio - 3.0).abs() < 0.001);
        assert!((summary.overall.median_ratio - 2.0).abs() < 0.001);
    }

    #[test]
    fn prior_prefers_the_matching_complexity_then_overall() {
        let store = StatsStore {
            records: vec![
                sample(Complexity::Low, 100, 100),
                sample(Complexity::High, 100, 300),
            ],
            ..StatsStore::default()
        };
        let summary = summarize(&store);
        assert!((prior_for(&summary, Complexity::Low) - 1.0).abs() < 0.001);
        assert!((prior_for(&summary, Complexity::High) - 3.0).abs() < 0.001);
        // No medium samples: overall median.
        assert!((prior_for(&summary, Complexity::Medium) - 2.0).abs() < 0.001);
        assert_eq!(prior_for(&Summary::default(), Complexity::Medium), 1.0);
    }

    #[test]
    fn prior_for_spec_is_weighted() {
        let mut spec = ProjectSpec::from_idea(
            &Idea {
                title: Some("X".into()),
                details: "x".into(),
                raw: String::new(),
            },
            "x".into(),
        );
        spec.milestones = vec![
            MilestoneSpec {
                id: "m1".into(),
                title: "a".into(),
                description: String::new(),
                todos: vec![],
                complexity: Complexity::Low,
                algorithmic_complexity: String::new(),
                estimate_secs: 600,
            },
            MilestoneSpec {
                id: "m2".into(),
                title: "b".into(),
                description: String::new(),
                todos: vec![],
                complexity: Complexity::High,
                algorithmic_complexity: String::new(),
                estimate_secs: 600,
            },
        ];
        let store = StatsStore {
            records: vec![
                sample(Complexity::Low, 100, 100),
                sample(Complexity::High, 100, 300),
            ],
            ..StatsStore::default()
        };
        let summary = summarize(&store);
        // low weight 1 @1x, high weight 3 @3x -> (1 + 9) / 4 = 2.5
        assert!((prior_for_spec(&summary, &spec) - 2.5).abs() < 0.001);
    }

    #[test]
    fn prompt_line_is_empty_until_there_are_samples() {
        assert!(prompt_line(&Summary::default()).is_none());
        let store = StatsStore {
            records: vec![sample(Complexity::Medium, 100, 150)],
            ..StatsStore::default()
        };
        let line = prompt_line(&summarize(&store)).unwrap();
        assert!(line.contains("1 completed"));
        assert!(line.contains("medium"));
    }

    #[test]
    fn record_persists_under_lock_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            state_dir: dir.path().display().to_string(),
            ..Config::default()
        };
        let mut milestone = Milestone {
            id: "m1".into(),
            title: "Foundation".into(),
            description: String::new(),
            todos: vec![],
            complexity: Complexity::Medium,
            algorithmic_complexity: String::new(),
            estimate_secs: 600,
            status: MilestoneStatus::Done,
            started_at: None,
            finished_at: None,
            actual_secs: 900,
            note: String::new(),
        };
        let host = HostProfile {
            cpu_model: "X".into(),
            cores: 8,
            physical_cores: 4,
            cpu_mhz: 3500.0,
            mem_mb: 8000,
            arch: "x86_64".into(),
        };
        assert!(record(&cfg, "demo", &milestone, &host, t("2026-10-20T12:00:00Z")).unwrap());
        let store = load(&cfg.stats_file()).unwrap();
        assert_eq!(store.records.len(), 1);
        assert_eq!(store.records[0].actual_secs, 900);
        assert_eq!(store.records[0].cpu_ghz, 3.5);
        assert!(!cfg.state_dir_path().join("stats.lock").exists());

        // Zero durations are ignored.
        milestone.actual_secs = 0;
        assert!(!record(&cfg, "demo", &milestone, &host, t("2026-10-20T12:00:00Z")).unwrap());
        assert_eq!(load(&cfg.stats_file()).unwrap().records.len(), 1);
    }

    #[test]
    fn render_mentions_ratios_or_empty_state() {
        assert!(render(&Summary::default()).contains("no completed milestones"));
        let store = StatsStore {
            records: vec![sample(Complexity::High, 100, 250)],
            ..StatsStore::default()
        };
        let text = render(&summarize(&store));
        assert!(text.contains("overall"));
        assert!(text.contains("high"));
        assert!(text.contains("2.50"));
    }
}
