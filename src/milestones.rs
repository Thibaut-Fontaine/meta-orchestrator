//! Milestones, live progress and ETA estimation.
//!
//! Each project gets a `MILESTONES.json` board (committed in its repository).
//! Agents move milestones through `pending → in_progress → done` with the
//! `mox milestone` command; `mox status` reads the board and estimates when the
//! current project (or the next milestone) will finish.
//!
//! The ETA is weighted by milestone complexity and calibrated with the actual
//! time spent on completed milestones, so a project that runs slower or faster
//! than planned self-corrects.

use crate::cli::MilestoneAction;
use crate::config::Config;
use crate::exec::{CommandRunner, RealRunner};
use crate::host::HostProfile;
use crate::model::{Complexity, MilestoneSpec, ProjectSpec};
use crate::notify::{self, Event, EventKind};
use crate::stats;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const BOARD_FILE: &str = "MILESTONES.json";
pub const BOARD_VERSION: u32 = 1;
/// When a milestone has no estimate, assume this many seconds per weight unit.
pub const FALLBACK_SECS_PER_WEIGHT: u64 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MilestoneStatus {
    #[default]
    Pending,
    InProgress,
    Done,
    Blocked,
    Skipped,
}

impl MilestoneStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MilestoneStatus::Pending => "pending",
            MilestoneStatus::InProgress => "in-progress",
            MilestoneStatus::Done => "done",
            MilestoneStatus::Blocked => "blocked",
            MilestoneStatus::Skipped => "skipped",
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(
            self,
            MilestoneStatus::Pending | MilestoneStatus::InProgress | MilestoneStatus::Blocked
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub complexity: Complexity,
    #[serde(default)]
    pub algorithmic_complexity: String,
    #[serde(default)]
    pub estimate_secs: u64,
    #[serde(default)]
    pub status: MilestoneStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub actual_secs: u64,
    #[serde(default)]
    pub note: String,
}

impl Milestone {
    pub fn open_todos(&self) -> usize {
        self.todos.iter().filter(|t| !t.done).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneEvent {
    pub at: DateTime<Utc>,
    pub milestone: String,
    /// started, done, blocked, skipped, todo, todo-added
    pub kind: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    #[serde(default = "board_version")]
    pub version: u32,
    pub slug: String,
    pub generated_at: DateTime<Utc>,
    #[serde(default)]
    pub milestones: Vec<Milestone>,
    #[serde(default)]
    pub events: Vec<MilestoneEvent>,
    /// Starting calibration for a brand-new project, learned from completed
    /// milestones across all projects. 1.0 when there is no history.
    #[serde(default = "default_prior")]
    pub prior_factor: f64,
}

fn default_prior() -> f64 {
    1.0
}

fn board_version() -> u32 {
    BOARD_VERSION
}

/// Build the initial board from a plan specification.
pub fn board_from_spec(spec: &ProjectSpec, now: DateTime<Utc>) -> Board {
    board_from_spec_with_prior(spec, now, 1.0)
}

/// Build the initial board with a cross-project prior calibration.
pub fn board_from_spec_with_prior(spec: &ProjectSpec, now: DateTime<Utc>, prior: f64) -> Board {
    let specs = if spec.milestones.is_empty() {
        fallback_for(spec)
    } else {
        spec.milestones.clone()
    };
    Board {
        version: BOARD_VERSION,
        slug: spec.slug.clone(),
        generated_at: now,
        milestones: specs
            .into_iter()
            .map(|m| Milestone {
                id: m.id,
                title: m.title,
                description: m.description,
                todos: m
                    .todos
                    .into_iter()
                    .map(|text| Todo { text, done: false })
                    .collect(),
                complexity: m.complexity,
                algorithmic_complexity: m.algorithmic_complexity,
                estimate_secs: m.estimate_secs,
                status: MilestoneStatus::Pending,
                started_at: None,
                finished_at: None,
                actual_secs: 0,
                note: String::new(),
            })
            .collect(),
        events: Vec::new(),
        prior_factor: prior.clamp(0.25, 4.0),
    }
}

/// Sensible milestone plan when the brain did not provide one.
pub fn fallback_for(spec: &ProjectSpec) -> Vec<MilestoneSpec> {
    let mut implementation: Vec<String> = if spec.acceptance_criteria.is_empty() {
        vec![
            "Implement the core behaviour".to_string(),
            "Handle errors and edge cases".to_string(),
        ]
    } else {
        spec.acceptance_criteria.clone()
    };
    implementation.push("Write/adjust unit tests for the behaviour".to_string());
    vec![
        MilestoneSpec {
            id: "m1".to_string(),
            title: "Foundation and failing tests".to_string(),
            description: "Set up the project skeleton and write the first failing tests."
                .to_string(),
            todos: vec![
                "Write failing unit tests for the core behaviour".to_string(),
                "Set up the project structure and dependencies".to_string(),
            ],
            complexity: Complexity::Medium,
            algorithmic_complexity: String::new(),
            estimate_secs: 1800,
        },
        MilestoneSpec {
            id: "m2".to_string(),
            title: "Implement the core".to_string(),
            description: "Make the tests pass with the simplest correct implementation."
                .to_string(),
            todos: implementation,
            complexity: Complexity::High,
            algorithmic_complexity: String::new(),
            estimate_secs: 3600,
        },
        MilestoneSpec {
            id: "m3".to_string(),
            title: "Validate, document and ship".to_string(),
            description: "Run the full suite, document usage, and push a clean tree.".to_string(),
            todos: vec![
                "Run the full test suite, formatter and linter".to_string(),
                "Write README usage notes".to_string(),
                "Commit everything and push".to_string(),
            ],
            complexity: Complexity::Low,
            algorithmic_complexity: String::new(),
            estimate_secs: 1200,
        },
    ]
}

/// Load a board, or `None` when the file does not exist.
pub fn load_optional(path: &Path) -> Option<Board> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).ok(),
        Err(_) => None,
    }
}

pub fn load(path: &Path) -> Result<Board> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let board: Board =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(board)
}

/// Atomically save a board (normal permissions: it lives in the repository).
pub fn save(path: &Path, board: &Board) -> Result<()> {
    let json = serde_json::to_string_pretty(board)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Board path for a project directory.
pub fn board_path(project_dir: &Path) -> PathBuf {
    project_dir.join(BOARD_FILE)
}

fn find_mut<'a>(board: &'a mut Board, id: &str) -> Result<&'a mut Milestone> {
    let index = board.milestones.iter().position(|m| m.id == id);
    let Some(index) = index else {
        let known = board
            .milestones
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("unknown milestone `{id}` (known: {known})");
    };
    Ok(&mut board.milestones[index])
}

fn event(board: &mut Board, id: &str, kind: &str, note: &str, now: DateTime<Utc>) {
    board.events.push(MilestoneEvent {
        at: now,
        milestone: id.to_string(),
        kind: kind.to_string(),
        note: note.to_string(),
    });
}

pub fn start(board: &mut Board, id: &str, now: DateTime<Utc>) -> Result<()> {
    let milestone = find_mut(board, id)?;
    if milestone.status == MilestoneStatus::Done {
        bail!("milestone `{id}` is already done");
    }
    milestone.status = MilestoneStatus::InProgress;
    if milestone.started_at.is_none() {
        milestone.started_at = Some(now);
    }
    event(board, id, "started", "", now);
    Ok(())
}

pub fn done(board: &mut Board, id: &str, note: &str, now: DateTime<Utc>) -> Result<()> {
    let milestone = find_mut(board, id)?;
    if milestone.status == MilestoneStatus::Done {
        return Ok(());
    }
    milestone.status = MilestoneStatus::Done;
    milestone.finished_at = Some(now);
    milestone.actual_secs = milestone
        .started_at
        .map(|start| now.signed_duration_since(start).num_seconds().max(0) as u64)
        .unwrap_or(0);
    if !note.trim().is_empty() {
        milestone.note = note.trim().to_string();
    }
    event(board, id, "done", note.trim(), now);
    Ok(())
}

pub fn block(board: &mut Board, id: &str, note: &str, now: DateTime<Utc>) -> Result<()> {
    let milestone = find_mut(board, id)?;
    if milestone.status == MilestoneStatus::Done {
        bail!("milestone `{id}` is already done");
    }
    milestone.status = MilestoneStatus::Blocked;
    if !note.trim().is_empty() {
        milestone.note = note.trim().to_string();
    }
    event(board, id, "blocked", note.trim(), now);
    Ok(())
}

pub fn skip(board: &mut Board, id: &str, now: DateTime<Utc>) -> Result<()> {
    let milestone = find_mut(board, id)?;
    if milestone.status == MilestoneStatus::Done {
        bail!("milestone `{id}` is already done");
    }
    milestone.status = MilestoneStatus::Skipped;
    event(board, id, "skipped", "", now);
    Ok(())
}

pub fn set_todo(
    board: &mut Board,
    id: &str,
    index: usize,
    done: bool,
    now: DateTime<Utc>,
) -> Result<()> {
    let milestone = find_mut(board, id)?;
    let todo = milestone
        .todos
        .get_mut(index)
        .with_context(|| format!("milestone `{id}` has no todo #{}", index + 1))?;
    todo.done = done;
    let note = format!(
        "todo #{} {}: {}",
        index + 1,
        if done { "done" } else { "reopened" },
        todo.text
    );
    event(board, id, "todo", &note, now);
    Ok(())
}

pub fn add_todo(board: &mut Board, id: &str, text: &str, now: DateTime<Utc>) -> Result<()> {
    if text.trim().is_empty() {
        bail!("todo text must not be empty");
    }
    let milestone = find_mut(board, id)?;
    milestone.todos.push(Todo {
        text: text.trim().to_string(),
        done: false,
    });
    event(board, id, "todo-added", text.trim(), now);
    Ok(())
}

/// Live progress and ETA for one project.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Progress {
    pub slug: String,
    pub total: usize,
    pub done: usize,
    pub done_weight: u32,
    pub total_weight: u32,
    pub percent: u8,
    pub current: Option<String>,
    pub current_title: String,
    pub current_started_at: Option<DateTime<Utc>>,
    pub remaining_secs: u64,
    pub eta: Option<DateTime<Utc>>,
    /// Seconds left in the current milestone (the task the agent is on now).
    pub current_remaining_secs: Option<u64>,
    /// When the current milestone should end.
    pub current_eta: Option<DateTime<Utc>>,
    pub calibration: f64,
    pub blocked: bool,
    pub complete: bool,
    pub last_event_at: Option<DateTime<Utc>>,
    pub last_event: Option<String>,
}

impl Progress {
    pub fn eta_label(&self) -> String {
        if self.complete {
            return "done".to_string();
        }
        if self.blocked {
            return "blocked".to_string();
        }
        match self.eta {
            Some(eta) => format!(
                "{} (in {})",
                eta.with_timezone(&Local).format("%H:%M"),
                crate::util::format_secs(self.remaining_secs)
            ),
            None => "-".to_string(),
        }
    }

    /// Compact ETA for the status table: `current/project` in local time.
    pub fn compact_eta(&self) -> String {
        if self.complete {
            return "done".to_string();
        }
        if self.blocked {
            return "blocked".to_string();
        }
        let fmt = |when: DateTime<Utc>| when.with_timezone(&Local).format("%H:%M").to_string();
        match (self.current_eta, self.eta) {
            (Some(current), Some(project)) if current != project => {
                format!("{}/{}", fmt(current), fmt(project))
            }
            (_, Some(project)) => fmt(project),
            _ => "-".to_string(),
        }
    }

    /// Short label for the status table, e.g. `m2/5 40%`.
    pub fn short(&self) -> String {
        if self.total == 0 {
            return "-".to_string();
        }
        let current = self.current.as_deref().unwrap_or("-");
        format!("{}/{} {}%", current, self.total, self.percent)
    }
}

/// Compute weighted progress and an ETA.
///
/// * progress is weighted by [`Complexity::weight`];
/// * completed milestones calibrate the estimate (actual / planned, clamped to
///   0.25–4×) so a slow or fast project self-corrects;
/// * the current milestone's elapsed time is subtracted from its estimate.
pub fn progress(board: &Board, now: DateTime<Utc>) -> Progress {
    let active: Vec<&Milestone> = board
        .milestones
        .iter()
        .filter(|m| m.status != MilestoneStatus::Skipped)
        .collect();
    let total = active.len();
    let mut total_weight = 0u32;
    let mut done_weight = 0u32;
    let mut done = 0usize;
    let mut completed_planned = 0u64;
    let mut completed_actual = 0u64;
    let mut blocked = false;

    for milestone in &active {
        total_weight += milestone.complexity.weight();
        if milestone.status == MilestoneStatus::Done {
            done += 1;
            done_weight += milestone.complexity.weight();
            completed_planned += milestone.estimate_secs;
            completed_actual += milestone.actual_secs;
        }
        if milestone.status == MilestoneStatus::Blocked {
            blocked = true;
        }
    }

    let calibration = if completed_planned > 0 && completed_actual > 0 {
        (completed_actual as f64 / completed_planned as f64).clamp(0.25, 4.0)
    } else {
        board.prior_factor.clamp(0.25, 4.0)
    };

    let mut remaining = 0u64;
    for milestone in &active {
        if !milestone.status.is_open() {
            continue;
        }
        if milestone.status == MilestoneStatus::Blocked {
            continue;
        }
        let base = if milestone.estimate_secs > 0 {
            milestone.estimate_secs as f64
        } else {
            (milestone.complexity.weight() as u64 * FALLBACK_SECS_PER_WEIGHT) as f64
        };
        let mut effective = base;
        if milestone.status == MilestoneStatus::InProgress {
            if let Some(started) = milestone.started_at {
                let elapsed = now.signed_duration_since(started).num_seconds().max(0) as f64;
                // Keep a small floor so a late milestone still gets an ETA.
                effective = (base - elapsed).max(60.0f64.min(base));
            }
        }
        remaining = remaining.saturating_add((effective * calibration).round() as u64);
    }

    let current = active
        .iter()
        .find(|m| m.status == MilestoneStatus::InProgress)
        .or_else(|| active.iter().find(|m| m.status == MilestoneStatus::Pending));

    let complete = total > 0 && done == total;
    let eta = if remaining > 0 && !blocked && !complete {
        Some(now + Duration::seconds(remaining as i64))
    } else {
        None
    };
    let percent = if total_weight == 0 {
        0
    } else {
        ((done_weight as f64 / total_weight as f64) * 100.0).round() as u8
    };

    // ETA for the milestone the agent is working on right now.
    let current_remaining_secs = current.and_then(|milestone| {
        if milestone.status == MilestoneStatus::Blocked {
            return None;
        }
        let base = if milestone.estimate_secs > 0 {
            milestone.estimate_secs as f64
        } else {
            (milestone.complexity.weight() as u64 * FALLBACK_SECS_PER_WEIGHT) as f64
        };
        let mut effective = base;
        if milestone.status == MilestoneStatus::InProgress {
            if let Some(started) = milestone.started_at {
                let elapsed = now.signed_duration_since(started).num_seconds().max(0) as f64;
                effective = (base - elapsed).max(60.0f64.min(base));
            }
        }
        Some((effective * calibration).round() as u64)
    });
    let current_eta = if blocked {
        None
    } else {
        current_remaining_secs.and_then(|secs| {
            if secs > 0 {
                Some(now + Duration::seconds(secs as i64))
            } else {
                None
            }
        })
    };
    let last_event = board.events.last();

    Progress {
        slug: board.slug.clone(),
        total,
        done,
        done_weight,
        total_weight,
        percent,
        current: current.map(|m| m.id.clone()),
        current_title: current.map(|m| m.title.clone()).unwrap_or_default(),
        current_started_at: current.and_then(|m| m.started_at),
        remaining_secs: remaining,
        eta,
        current_remaining_secs,
        current_eta,
        calibration,
        blocked,
        complete,
        last_event_at: last_event.map(|e| e.at),
        last_event: last_event.map(|e| {
            if e.note.is_empty() {
                format!("{} {}", e.milestone, e.kind)
            } else {
                format!("{} {} — {}", e.milestone, e.kind, e.note)
            }
        }),
    }
}

/// Detailed status for `mox milestone` / `mox milestone status`.
pub fn render_progress(board: &Board, now: DateTime<Utc>) -> String {
    let progress = progress(board, now);
    let mut out = String::new();
    out.push_str(&format!(
        "{} — {}/{} milestones · {}% (weight {}/{})\n",
        progress.slug,
        progress.done,
        progress.total,
        progress.percent,
        progress.done_weight,
        progress.total_weight
    ));
    if progress.complete {
        out.push_str("all milestones done.\n");
    } else if let Some(current) = &progress.current {
        let milestone = board.milestones.iter().find(|m| &m.id == current);
        if let Some(milestone) = milestone {
            out.push_str(&format!(
                "current: {} {} [{}] ~{}\n",
                milestone.id,
                milestone.title,
                milestone.complexity.as_str(),
                crate::util::format_secs(milestone.estimate_secs)
            ));
            if let Some(started) = milestone.started_at {
                let elapsed = now.signed_duration_since(started).num_seconds().max(0) as u64;
                out.push_str(&format!(
                    "started: {} ago\n",
                    crate::util::format_secs(elapsed)
                ));
            }
            if !milestone.algorithmic_complexity.is_empty() {
                out.push_str(&format!(
                    "algorithm: {}\n",
                    milestone.algorithmic_complexity
                ));
            }
            let open = milestone.open_todos();
            if open > 0 {
                out.push_str(&format!("todos left: {open}\n"));
            }
        }
        if let (Some(current_eta), Some(secs)) =
            (progress.current_eta, progress.current_remaining_secs)
        {
            out.push_str(&format!(
                "current milestone eta: {} (in {})\n",
                current_eta.with_timezone(&Local).format("%H:%M"),
                crate::util::format_secs(secs)
            ));
        }
        out.push_str(&format!(
            "project eta: {} · speed ×{:.2}\n",
            progress.eta_label(),
            progress.calibration
        ));
        out.push_str(&format!(
            "remaining: {}\n",
            crate::util::format_secs(progress.remaining_secs)
        ));
    }
    if progress.blocked {
        out.push_str("warning: at least one milestone is blocked; ETA is unavailable.\n");
    }
    if let Some(last) = &progress.last_event {
        match progress.last_event_at {
            Some(at) => {
                let ago = now.signed_duration_since(at).num_seconds().max(0) as u64;
                out.push_str(&format!(
                    "last event: {} ({} ago) — {last}\n",
                    at.with_timezone(&Local).format("%H:%M"),
                    crate::util::format_secs(ago)
                ));
            }
            None => out.push_str(&format!("last event: {last}\n")),
        }
    }
    out.push_str(&format!("\nboard: {BOARD_FILE}\n"));
    out
}

/// Table of milestones for `mox milestone list`.
pub fn render_list(board: &Board) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<4} {:<12} {:<10} {:>12} {:>12}  {}\n",
        "id", "status", "complexity", "estimate", "actual", "title"
    ));
    for milestone in &board.milestones {
        let estimate = if milestone.estimate_secs == 0 {
            "-".to_string()
        } else {
            crate::util::format_secs(milestone.estimate_secs)
        };
        let actual = if milestone.actual_secs == 0 {
            "-".to_string()
        } else {
            crate::util::format_secs(milestone.actual_secs)
        };
        out.push_str(&format!(
            "{:<4} {:<12} {:<10} {:>12} {:>12}  {}\n",
            milestone.id,
            milestone.status.as_str(),
            milestone.complexity.as_str(),
            estimate,
            actual,
            crate::util::truncate_chars(&milestone.title, 60)
        ));
        if milestone.open_todos() > 0 {
            for (index, todo) in milestone.todos.iter().enumerate() {
                if !todo.done {
                    out.push_str(&format!("       {}. [ ] {}\n", index + 1, todo.text));
                }
            }
        }
        if !milestone.algorithmic_complexity.is_empty() {
            out.push_str(&format!(
                "       algorithm: {}\n",
                milestone.algorithmic_complexity
            ));
        }
        if milestone.status == MilestoneStatus::Blocked && !milestone.note.is_empty() {
            out.push_str(&format!("       blocked: {}\n", milestone.note));
        }
    }
    out
}

/// Entry point for the standalone `mox milestone` command.
pub fn run(action: Option<MilestoneAction>, file: Option<PathBuf>) -> Result<()> {
    let path = file.unwrap_or_else(|| PathBuf::from(BOARD_FILE));
    let mut board = load(&path)?;
    let now = Utc::now();
    let mut transition: Option<String> = None;
    let mut completed: Option<String> = None;

    match action {
        None | Some(MilestoneAction::Status) => {
            print!("{}", render_progress(&board, now));
        }
        Some(MilestoneAction::List) => {
            print!("{}", render_list(&board));
        }
        Some(MilestoneAction::Start { id }) => {
            start(&mut board, &id, now)?;
            save(&path, &board)?;
            transition = Some(format!("started {id}"));
        }
        Some(MilestoneAction::Done { id, note }) => {
            let was_done = board
                .milestones
                .iter()
                .any(|m| m.id == id && m.status == MilestoneStatus::Done);
            done(&mut board, &id, &note, now)?;
            save(&path, &board)?;
            transition = Some(format!("completed {id}"));
            if !was_done {
                completed = Some(id);
            }
        }
        Some(MilestoneAction::Block { id, note }) => {
            block(&mut board, &id, &note, now)?;
            save(&path, &board)?;
            transition = Some(format!("blocked {id}"));
        }
        Some(MilestoneAction::Skip { id }) => {
            skip(&mut board, &id, now)?;
            save(&path, &board)?;
            transition = Some(format!("skipped {id}"));
        }
        Some(MilestoneAction::Todo { id, index, undo }) => {
            if index == 0 {
                bail!("todo index is 1-based");
            }
            set_todo(&mut board, &id, index - 1, !undo, now)?;
            save(&path, &board)?;
            println!(
                "todo {index} {} for {id}",
                if undo { "reopened" } else { "done" }
            );
            println!("remember to commit MILESTONES.json with your work");
        }
        Some(MilestoneAction::AddTodo { id, text }) => {
            add_todo(&mut board, &id, &text, now)?;
            save(&path, &board)?;
            println!("added a todo to {id}");
            println!("remember to commit MILESTONES.json with your work");
        }
    }

    if let Some(verb) = transition {
        println!("{verb}");
        print!("{}", render_progress(&board, Utc::now()));
        notify_transition(&board, &verb);
    }
    if let Some(id) = completed {
        record_stats(&board, &id);
    }
    Ok(())
}

/// Append this completed milestone to the cross-project statistics.
fn record_stats(board: &Board, id: &str) {
    let Ok(cfg) = Config::load(None) else {
        return;
    };
    let Some(milestone) = board.milestones.iter().find(|m| m.id == id) else {
        return;
    };
    let host = HostProfile::detect();
    let _ = stats::record(&cfg, &board.slug, milestone, &host, Utc::now());
}

/// Best-effort Telegram/desktop notification on a milestone transition.
fn notify_transition(board: &Board, verb: &str) {
    let Ok(cfg) = Config::load(None) else {
        return;
    };
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::new());
    let notifier = notify::from_config(&cfg, runner);
    if notifier.name() == "none" {
        return;
    }
    let now = Utc::now();
    let progress = progress(board, now);
    let body = format!(
        "{} · {}/{} milestones · {}\nETA: {}",
        board.slug,
        progress.done,
        progress.total,
        progress.percent,
        progress.eta_label()
    );
    let _ = notifier.send(&Event::new(
        EventKind::Progress,
        format!("{} — {verb}", board.slug),
        body,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Idea, ProjectSpec};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        t("2026-10-20T12:00:00Z")
    }

    fn spec() -> ProjectSpec {
        ProjectSpec::from_idea(
            &Idea {
                title: Some("Demo".into()),
                details: "build a demo".into(),
                raw: String::new(),
            },
            "demo".into(),
        )
    }

    fn board() -> Board {
        let mut spec = spec();
        spec.milestones = vec![
            MilestoneSpec {
                id: "m1".into(),
                title: "Foundation".into(),
                description: String::new(),
                todos: vec!["write tests".into()],
                complexity: Complexity::Low,
                algorithmic_complexity: String::new(),
                estimate_secs: 600,
            },
            MilestoneSpec {
                id: "m2".into(),
                title: "Core".into(),
                description: String::new(),
                todos: vec![],
                complexity: Complexity::Medium,
                algorithmic_complexity: "O(n log n) per batch".into(),
                estimate_secs: 1200,
            },
            MilestoneSpec {
                id: "m3".into(),
                title: "Ship".into(),
                description: String::new(),
                todos: vec![],
                complexity: Complexity::High,
                algorithmic_complexity: String::new(),
                estimate_secs: 1800,
            },
        ];
        board_from_spec(&spec, now())
    }

    #[test]
    fn board_from_spec_creates_pending_milestones() {
        let board = board();
        assert_eq!(board.milestones.len(), 3);
        assert_eq!(board.slug, "demo");
        assert!(board
            .milestones
            .iter()
            .all(|m| m.status == MilestoneStatus::Pending));
        assert_eq!(board.milestones[0].todos.len(), 1);
        assert!(!board.milestones[0].todos[0].done);
    }

    #[test]
    fn fallback_plan_is_always_available() {
        let fallback = fallback_for(&spec());
        assert_eq!(fallback.len(), 3);
        assert!(fallback[0]
            .todos
            .iter()
            .any(|t| t.contains("failing unit tests")));
        assert!(fallback.iter().all(|m| m.estimate_secs > 0));
    }

    #[test]
    fn start_and_done_track_actual_time() {
        let mut board = board();
        start(&mut board, "m1", now()).unwrap();
        assert_eq!(board.milestones[0].status, MilestoneStatus::InProgress);
        assert_eq!(board.milestones[0].started_at, Some(now()));

        done(
            &mut board,
            "m1",
            "tests green",
            now() + Duration::minutes(5),
        )
        .unwrap();
        assert_eq!(board.milestones[0].status, MilestoneStatus::Done);
        assert_eq!(board.milestones[0].actual_secs, 300);
        assert_eq!(board.milestones[0].note, "tests green");
        assert_eq!(board.events.last().unwrap().kind, "done");
    }

    #[test]
    fn does_not_reopen_a_done_milestone() {
        let mut board = board();
        start(&mut board, "m1", now()).unwrap();
        done(&mut board, "m1", "", now()).unwrap();
        assert!(start(&mut board, "m1", now()).is_err());
        assert!(done(&mut board, "m1", "", now()).is_ok());
        assert_eq!(board.milestones[0].status, MilestoneStatus::Done);
    }

    #[test]
    fn todo_operations_are_one_based_in_the_cli_and_zero_based_here() {
        let mut board = board();
        set_todo(&mut board, "m1", 0, true, now()).unwrap();
        assert!(board.milestones[0].todos[0].done);
        assert!(set_todo(&mut board, "m1", 9, true, now()).is_err());
        add_todo(&mut board, "m1", "  extra  ", now()).unwrap();
        assert_eq!(board.milestones[0].todos[1].text, "extra");
    }

    #[test]
    fn progress_is_weighted_by_complexity() {
        let mut board = board();
        start(&mut board, "m1", now() - Duration::minutes(10)).unwrap();
        done(&mut board, "m1", "", now()).unwrap();
        let p = progress(&board, now());
        // Low weight 1 done of total 1+2+3 = 6.
        assert_eq!(p.total, 3);
        assert_eq!(p.done, 1);
        assert_eq!(p.done_weight, 1);
        assert_eq!(p.total_weight, 6);
        assert_eq!(p.percent, 17);
        assert_eq!(p.current.as_deref(), Some("m2"));
    }

    #[test]
    fn eta_calibrates_from_completed_milestones() {
        let mut board = board();
        start(&mut board, "m1", now() - Duration::minutes(10)).unwrap();
        done(&mut board, "m1", "", now()).unwrap();
        // Planned 600s, actual 600s -> calibration 1.0.
        // Remaining = 1200 + 1800 = 3000s.
        let p = progress(&board, now());
        assert!((p.calibration - 1.0).abs() < 0.001);
        assert_eq!(p.remaining_secs, 3000);
        assert_eq!(p.eta, Some(now() + Duration::seconds(3000)));
    }

    #[test]
    fn slow_project_increases_remaining_estimate() {
        let mut board = board();
        board.milestones[0].started_at = Some(now() - Duration::minutes(20));
        done(&mut board, "m1", "", now()).unwrap();
        // Planned 600s, actual 1200s -> 2x calibration.
        let p = progress(&board, now());
        assert!((p.calibration - 2.0).abs() < 0.001);
        assert_eq!(p.remaining_secs, 6000);
    }

    #[test]
    fn current_milestone_elapsed_time_reduces_remaining() {
        let mut board = board();
        start(&mut board, "m1", now() - Duration::minutes(5)).unwrap();
        let p = progress(&board, now());
        // 600 - 300 = 300 remaining for m1, plus 1200 + 1800.
        assert_eq!(p.remaining_secs, 3300);
        assert_eq!(p.current_remaining_secs, Some(300));
        assert_eq!(p.current_eta, Some(now() + Duration::seconds(300)));
        assert!(p.compact_eta().contains('/'));
    }

    #[test]
    fn blocked_milestone_makes_eta_unavailable() {
        let mut board = board();
        start(&mut board, "m2", now()).unwrap();
        block(&mut board, "m2", "waiting for an API key", now()).unwrap();
        let p = progress(&board, now());
        assert!(p.blocked);
        assert_eq!(p.eta, None);
        assert_eq!(p.eta_label(), "blocked");
    }

    #[test]
    fn all_done_is_complete() {
        let mut board = board();
        for id in ["m1", "m2", "m3"] {
            start(&mut board, id, now()).unwrap();
            done(&mut board, id, "", now()).unwrap();
        }
        let p = progress(&board, now());
        assert!(p.complete);
        assert_eq!(p.percent, 100);
        assert_eq!(p.eta, None);
        assert_eq!(p.eta_label(), "done");
        assert_eq!(p.short(), "-/3 100%");
    }

    #[test]
    fn prior_factor_calibrates_a_brand_new_project() {
        let mut spec = spec();
        spec.milestones = board()
            .milestones
            .iter()
            .map(|m| crate::model::MilestoneSpec {
                id: m.id.clone(),
                title: m.title.clone(),
                description: m.description.clone(),
                todos: m.todos.iter().map(|t| t.text.clone()).collect(),
                complexity: m.complexity,
                algorithmic_complexity: m.algorithmic_complexity.clone(),
                estimate_secs: m.estimate_secs,
            })
            .collect();
        let board = board_from_spec_with_prior(&spec, now(), 2.0);
        assert_eq!(board.prior_factor, 2.0);
        let p = progress(&board, now());
        assert!((p.calibration - 2.0).abs() < 0.001);
        assert_eq!(p.remaining_secs, 7200);
        assert_eq!(p.current_remaining_secs, Some(1200));
    }

    #[test]
    fn missing_estimate_uses_complexity_weight() {
        let mut board = board();
        board.milestones[1].estimate_secs = 0;
        let p = progress(&board, now());
        // m1 600 + m2 (2*1800) + m3 1800
        assert_eq!(p.remaining_secs, 600 + 3600 + 1800);
    }

    #[test]
    fn skipped_milestones_are_excluded() {
        let mut board = board();
        skip(&mut board, "m1", now()).unwrap();
        let p = progress(&board, now());
        assert_eq!(p.total, 2);
        assert_eq!(p.total_weight, 5);
        assert_eq!(p.current.as_deref(), Some("m2"));
    }

    #[test]
    fn renders_progress_and_list() {
        let mut board = board();
        start(&mut board, "m2", now() - Duration::minutes(3)).unwrap();
        let text = render_progress(&board, now());
        assert!(text.contains("demo — 0/3 milestones"));
        assert!(text.contains("current: m2 Core"));
        assert!(text.contains("O(n log n)"));
        assert!(text.contains("eta:"));
        assert!(text.contains("last event:"));
        let list = render_list(&board);
        assert!(list.contains("m1"));
        assert!(list.contains("write tests"));
        assert!(list.contains("O(n log n) per batch"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BOARD_FILE);
        let mut board = board();
        start(&mut board, "m1", now()).unwrap();
        save(&path, &board).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, board);
        assert!(load_optional(&path).is_some());
        assert!(load_optional(&dir.path().join("nope.json")).is_none());
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec![BOARD_FILE]);
    }

    #[test]
    fn errors_are_precise() {
        let mut board = board();
        let err = start(&mut board, "nope", now()).unwrap_err().to_string();
        assert!(err.contains("unknown milestone"));
        let err = add_todo(&mut board, "m1", "  ", now())
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"));
    }
}
