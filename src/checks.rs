//! Human attention queue: "what should we check now, and what later, and when".
//!
//! Projects that touch the VPS (fetchers, APIs, services) often need a human to
//! verify something: a cron actually started, a key is configured, a deploy is
//! live, a first batch of data looks sane. Agents cannot do those checks, so
//! they record them in `CHECKS.md` inside their project, and `mox check`
//! aggregates everything into one time-ordered dashboard.
//!
//! The queue is local, dependency-free and fully deterministic with an injected
//! `now`, which keeps the time logic testable.

use crate::exec::{CommandRunner, CommandSpec};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const STORE_VERSION: u32 = 1;

/// What kind of human action is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    /// Something running on the VPS (cron, systemd unit, fetcher, disk).
    Vps,
    /// An external API/key/quota that must be verified.
    Api,
    /// A deployment or release step.
    Deploy,
    /// A generic manual verification (UI, output, data).
    #[default]
    Manual,
    /// Data quality / first batch review.
    Data,
    /// Credentials, permissions or exposure review.
    Security,
    Other,
}

impl CheckKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckKind::Vps => "vps",
            CheckKind::Api => "api",
            CheckKind::Deploy => "deploy",
            CheckKind::Manual => "manual",
            CheckKind::Data => "data",
            CheckKind::Security => "security",
            CheckKind::Other => "other",
        }
    }
}

impl std::str::FromStr for CheckKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vps" | "server" | "ssh" => Ok(CheckKind::Vps),
            "api" | "http" | "external" => Ok(CheckKind::Api),
            "deploy" | "release" | "ci" => Ok(CheckKind::Deploy),
            "manual" | "human" | "ui" => Ok(CheckKind::Manual),
            "data" | "dataset" => Ok(CheckKind::Data),
            "security" | "secret" | "auth" => Ok(CheckKind::Security),
            "other" => Ok(CheckKind::Other),
            other => Err(format!(
                "unknown check kind `{other}` (use vps, api, deploy, manual, data, security, other)"
            )),
        }
    }
}

/// How urgent a check is, independent of its due date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

impl Priority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Critical => "critical",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Priority::Low => 1,
            Priority::Normal => 2,
            Priority::High => 3,
            Priority::Critical => 4,
        }
    }
}

impl std::str::FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "someday" => Ok(Priority::Low),
            "normal" | "medium" | "default" => Ok(Priority::Normal),
            "high" | "important" => Ok(Priority::High),
            "critical" | "urgent" | "asap" => Ok(Priority::Critical),
            other => Err(format!(
                "unknown priority `{other}` (use low, normal, high, critical)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    #[default]
    Open,
    Done,
}

/// One human check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub kind: CheckKind,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub status: CheckStatus,
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub done_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub done_note: String,
    /// Recurrence in seconds; when set, `done` reschedules instead of closing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    /// Read-only VPS probe, e.g. `systemctl is-active fetcher`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_at: Option<DateTime<Utc>>,
}

impl Check {
    pub fn new(
        slug: impl Into<String>,
        title: impl Into<String>,
        kind: CheckKind,
        priority: Priority,
        due_at: Option<DateTime<Utc>>,
        detail: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Self {
        let slug = slug.into();
        let title = title.into();
        Check {
            id: short_id(&format!("{slug}|{title}")),
            slug,
            title,
            detail: detail.into(),
            kind,
            priority,
            status: CheckStatus::Open,
            due_at,
            created_at: now,
            updated_at: now,
            done_at: None,
            done_note: String::new(),
            every_secs: None,
            probe: None,
            probe_note: None,
            probe_at: None,
        }
    }

    pub fn with_probe(mut self, probe: Option<String>) -> Self {
        self.probe = probe;
        self
    }
}

/// Time bucket used by the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Overdue,
    Now,
    Today,
    Soon,
    Later,
    Unscheduled,
}

impl Bucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            Bucket::Overdue => "overdue",
            Bucket::Now => "now",
            Bucket::Today => "today",
            Bucket::Soon => "soon",
            Bucket::Later => "later",
            Bucket::Unscheduled => "unscheduled",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Bucket::Overdue => "OVERDUE",
            Bucket::Now => "NOW",
            Bucket::Today => "TODAY",
            Bucket::Soon => "SOON",
            Bucket::Later => "LATER",
            Bucket::Unscheduled => "UNSCHEDULED",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Bucket::Overdue => 0,
            Bucket::Now => 1,
            Bucket::Today => 2,
            Bucket::Soon => 3,
            Bucket::Later => 4,
            Bucket::Unscheduled => 5,
        }
    }
}

/// A check plus its computed bucket and countdown, suitable for JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct CheckView {
    #[serde(flatten)]
    pub check: Check,
    pub bucket: String,
    pub due_in_seconds: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub checks: Vec<Check>,
}

fn default_version() -> u32 {
    STORE_VERSION
}

impl CheckStore {
    pub fn open(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Open)
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }

    /// Add a check. Re-adding the same project+title while it is still open
    /// refreshes the existing entry instead of duplicating it.
    pub fn add(&mut self, mut check: Check) -> String {
        if let Some(existing) = self.checks.iter_mut().find(|c| {
            c.status == CheckStatus::Open && c.slug == check.slug && c.title == check.title
        }) {
            existing.detail = check.detail;
            existing.kind = check.kind;
            existing.priority = check.priority;
            existing.due_at = check.due_at;
            existing.every_secs = check.every_secs;
            existing.probe = check.probe;
            existing.updated_at = check.created_at;
            return existing.id.clone();
        }
        check.id = self.unique_id(&check.id, &check.slug, &check.title);
        let id = check.id.clone();
        self.checks.push(check);
        id
    }

    fn unique_id(&self, base: &str, slug: &str, title: &str) -> String {
        let mut candidate = base.to_string();
        let mut counter = 2;
        while let Some(existing) = self.checks.iter().find(|c| c.id == candidate) {
            if existing.slug == slug && existing.title == title {
                return candidate;
            }
            candidate = format!("{base}-{counter}");
            counter += 1;
        }
        candidate
    }

    pub fn mark_done(&mut self, id: &str, note: &str, now: DateTime<Utc>) -> Result<()> {
        let Some(check) = self.checks.iter_mut().find(|c| c.id == id) else {
            bail!("unknown check `{id}`");
        };
        if let Some(every) = check.every_secs.filter(|e| *e > 0) {
            check.status = CheckStatus::Open;
            check.due_at = Some(now + Duration::seconds(every as i64));
            check.done_at = Some(now);
            check.probe_note = None;
            check.updated_at = now;
            if !note.trim().is_empty() {
                check.done_note = note.trim().to_string();
            }
            return Ok(());
        }
        check.status = CheckStatus::Done;
        check.done_at = Some(now);
        check.updated_at = now;
        if !note.trim().is_empty() {
            check.done_note = note.trim().to_string();
        }
        Ok(())
    }

    pub fn reschedule(
        &mut self,
        id: &str,
        due: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let Some(check) = self.checks.iter_mut().find(|c| c.id == id) else {
            bail!("unknown check `{id}`");
        };
        if check.status == CheckStatus::Done {
            bail!("check `{id}` is already done; add a new one instead");
        }
        check.due_at = due;
        check.updated_at = now;
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        let before = self.checks.len();
        self.checks.retain(|c| c.id != id);
        if self.checks.len() == before {
            bail!("unknown check `{id}`");
        }
        Ok(())
    }

    pub fn reopen(&mut self, id: &str, now: DateTime<Utc>) -> Result<()> {
        let Some(check) = self.checks.iter_mut().find(|c| c.id == id) else {
            bail!("unknown check `{id}`");
        };
        check.status = CheckStatus::Open;
        check.done_at = None;
        check.done_note.clear();
        check.updated_at = now;
        Ok(())
    }
}

/// Load the queue; a missing file is an empty queue.
pub fn load(path: &Path) -> Result<CheckStore> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mut store: CheckStore = serde_json::from_str(&text)
                .with_context(|| format!("parsing checks file {}", path.display()))?;
            store.version = STORE_VERSION;
            Ok(store)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(CheckStore {
            version: STORE_VERSION,
            updated_at: crate::util::now_rfc3339(),
            checks: Vec::new(),
        }),
        Err(err) => Err(err).with_context(|| format!("reading checks file {}", path.display())),
    }
}

/// Atomically save the queue.
pub fn save(path: &Path, store: &CheckStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut to_write = store.clone();
    to_write.version = STORE_VERSION;
    to_write.updated_at = crate::util::now_rfc3339();
    let json = serde_json::to_string_pretty(&to_write)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    crate::util::write_private(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Parse a duration (`30m`, `2h`, `3d`, `1w`) into seconds.
pub fn parse_span(input: &str) -> Result<u64> {
    let text = input.trim().to_ascii_lowercase();
    let text = text.strip_prefix("every ").unwrap_or(&text).trim();
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.');
    let Some(index) = split else {
        bail!("duration `{input}` has no unit (use s, m, h, d or w)");
    };
    let number: f64 = text[..index]
        .parse()
        .with_context(|| format!("invalid number in duration `{input}`"))?;
    if !number.is_finite() || number <= 0.0 {
        bail!("duration `{input}` must be positive");
    }
    let seconds = match text[index..].trim() {
        "s" | "sec" | "secs" | "seconds" => number,
        "m" | "min" | "mins" | "minutes" => number * 60.0,
        "h" | "hr" | "hrs" | "hours" => number * 3600.0,
        "d" | "day" | "days" => number * 86_400.0,
        "w" | "week" | "weeks" => number * 604_800.0,
        other => bail!("unknown unit `{other}` in duration `{input}`"),
    };
    Ok(seconds.round() as u64)
}

/// Parse `+2h`, `90m`, `3d`, `1w`, `now`, or an RFC3339 timestamp.
pub fn parse_due(input: &str, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let text = input.trim();
    if text.is_empty() || matches!(text.to_ascii_lowercase().as_str(), "none" | "-" | "later") {
        return Ok(None);
    }
    let lower = text.to_ascii_lowercase();
    if matches!(lower.as_str(), "now" | "asap") {
        return Ok(Some(now));
    }
    if let Ok(absolute) = DateTime::parse_from_rfc3339(text) {
        return Ok(Some(absolute.with_timezone(&Utc)));
    }

    let relative = lower
        .strip_prefix("in ")
        .unwrap_or(&lower)
        .trim_start_matches('+')
        .trim();
    let split = relative.find(|c: char| !c.is_ascii_digit() && c != '.');
    let Some(index) = split else {
        bail!("due date `{input}` has no unit (use s, m, h, d or w)");
    };
    let (number, unit) = relative.split_at(index);
    let number: f64 = number
        .parse()
        .with_context(|| format!("invalid number in due date `{input}`"))?;
    if !number.is_finite() || number <= 0.0 {
        bail!("due date `{input}` must be in the future");
    }
    let seconds = match unit.trim() {
        "s" | "sec" | "secs" | "seconds" => number,
        "m" | "min" | "mins" | "minutes" => number * 60.0,
        "h" | "hr" | "hrs" | "hours" => number * 3600.0,
        "d" | "day" | "days" => number * 86_400.0,
        "w" | "week" | "weeks" => number * 604_800.0,
        other => bail!("unknown unit `{other}` in due date `{input}` (use s, m, h, d or w)"),
    };
    if seconds > 10.0 * 365.0 * 86_400.0 {
        bail!("due date `{input}` is more than ten years away");
    }
    Ok(Some(now + Duration::seconds(seconds.round() as i64)))
}

/// Assign a check to a time bucket.
pub fn bucket_for(check: &Check, now: DateTime<Utc>) -> Option<Bucket> {
    if check.status != CheckStatus::Open {
        return None;
    }
    let Some(due) = check.due_at else {
        return Some(if check.priority >= Priority::High {
            Bucket::Now
        } else {
            Bucket::Unscheduled
        });
    };
    if due < now {
        Some(Bucket::Overdue)
    } else if due <= now + Duration::hours(4) {
        Some(Bucket::Now)
    } else if due <= now + Duration::days(1) {
        Some(Bucket::Today)
    } else if due <= now + Duration::days(7) {
        Some(Bucket::Soon)
    } else {
        Some(Bucket::Later)
    }
}

/// Open checks ordered by urgency (bucket, priority, due date, project).
pub fn ordered_open(store: &CheckStore, now: DateTime<Utc>) -> Vec<(&Check, Bucket)> {
    let mut items: Vec<(&Check, Bucket)> = store
        .open()
        .into_iter()
        .filter_map(|check| bucket_for(check, now).map(|bucket| (check, bucket)))
        .collect();
    items.sort_by(|(a, ab), (b, bb)| {
        ab.rank()
            .cmp(&bb.rank())
            .then_with(|| b.priority.rank().cmp(&a.priority.rank()))
            .then_with(|| match (a.due_at, b.due_at) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.slug.cmp(&b.slug))
            .then_with(|| a.title.cmp(&b.title))
    });
    items
}

/// Human countdown, e.g. `in 3h`, `overdue 12m`, `in 2d`.
pub fn relative_label(check: &Check, now: DateTime<Utc>) -> String {
    let Some(due) = check.due_at else {
        return "unscheduled".to_string();
    };
    let seconds = due.signed_duration_since(now).num_seconds();
    if seconds < 0 {
        format!("overdue {}", format_span(-seconds))
    } else if seconds < 60 {
        "now".to_string()
    } else {
        format!("in {}", format_span(seconds))
    }
}

fn format_span(seconds: i64) -> String {
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        let rem = minutes % 60;
        return if rem == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{rem:02}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if days < 14 {
        if rem_hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{rem_hours}h")
        }
    } else {
        let weeks = days / 7;
        format!("{weeks}w")
    }
}

/// One-line summary for a status bar (`mox check --oneline`).
pub fn oneline(store: &CheckStore, now: DateTime<Utc>) -> String {
    let items = ordered_open(store, now);
    if items.is_empty() {
        return "checks: nothing to check".to_string();
    }
    let mut counts = [0usize; 6];
    for (_, bucket) in &items {
        counts[bucket.rank() as usize] += 1;
    }
    let mut parts = Vec::new();
    for bucket in [
        Bucket::Overdue,
        Bucket::Now,
        Bucket::Today,
        Bucket::Soon,
        Bucket::Later,
        Bucket::Unscheduled,
    ] {
        let n = counts[bucket.rank() as usize];
        if n > 0 {
            parts.push(format!("{n} {}", bucket.as_str()));
        }
    }
    format!("checks: {}", parts.join(" · "))
}

/// Render the dashboard/sidebar. Deterministic and terminal-width agnostic.
pub fn render(store: &CheckStore, now: DateTime<Utc>, include_done: bool) -> String {
    render_limited(store, now, include_done, None)
}

/// Same as [`render`] but only shows the first `limit` open checks.
pub fn render_limited(
    store: &CheckStore,
    now: DateTime<Utc>,
    include_done: bool,
    limit: Option<usize>,
) -> String {
    let mut items = ordered_open(store, now);
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    let mut out = String::new();
    out.push_str(&format!("CHECKS — {} UTC\n", now.format("%Y-%m-%d %H:%M")));
    if items.is_empty() {
        out.push_str("nothing to check.\n");
    } else {
        out.push_str(&format!(
            "{}\n",
            oneline(store, now).trim_start_matches("checks: ")
        ));
        let mut current: Option<Bucket> = None;
        for (check, bucket) in &items {
            if current != Some(*bucket) {
                out.push_str(&format!(
                    "\n{} ({})\n",
                    bucket.label(),
                    count_bucket(&items, *bucket)
                ));
                current = Some(*bucket);
            }
            let marker = match check.priority {
                Priority::Critical => "!!",
                Priority::High => "! ",
                _ => "  ",
            };
            out.push_str(&format!(
                "{marker}{id:<10} {slug:<20} {kind:<8} {title:<42} {when:<14} {prio}\n",
                id = check.id,
                slug = crate::util::truncate_chars(&check.slug, 20),
                kind = check.kind.as_str(),
                title = crate::util::truncate_chars(&check.title, 42),
                when = relative_label(check, now),
                prio = check.priority.as_str(),
            ));
            let mut notes = Vec::new();
            if !check.detail.trim().is_empty() {
                notes.push(format!("detail: {}", check.detail.trim()));
            }
            if let Some(every) = check.every_secs {
                notes.push(format!("every {}", crate::util::format_secs(every)));
            }
            if let Some(probe) = &check.probe {
                notes.push(format!("probe: {probe}"));
            }
            if let Some(note) = &check.probe_note {
                notes.push(format!("last probe: {note}"));
            }
            if !notes.is_empty() {
                out.push_str(&format!(
                    "  {}   {}\n",
                    " ".repeat(10 + 1 + 20 + 1 + 8 + 1),
                    crate::util::truncate_chars(&notes.join(" · "), 140)
                ));
            }
        }
    }

    if include_done {
        let mut done: Vec<&Check> = store
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Done)
            .collect();
        done.sort_by_key(|c| std::cmp::Reverse(c.done_at));
        if !done.is_empty() {
            out.push_str(&format!("\nDONE ({})\n", done.len()));
            for check in done {
                let when = check
                    .done_at
                    .map(|d| format!("done {}", relative_label_plain(d, now)))
                    .unwrap_or_else(|| "done".to_string());
                out.push_str(&format!(
                    "x {id:<10} {slug:<20} {kind:<8} {title:<42} {when}\n",
                    id = check.id,
                    slug = crate::util::truncate_chars(&check.slug, 20),
                    kind = check.kind.as_str(),
                    title = crate::util::truncate_chars(&check.title, 42),
                ));
            }
        }
    }

    out.push_str(
        "\ncommands: mox check done <id> · mox check later <id> 2h · mox check add …\n\
         live:     mox check --watch\n",
    );
    out
}

fn relative_label_plain(when: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(when).num_seconds();
    if seconds < 60 {
        "just now".to_string()
    } else {
        format!("{} ago", format_span(seconds))
    }
}

fn count_bucket(items: &[(&Check, Bucket)], bucket: Bucket) -> usize {
    items.iter().filter(|(_, b)| *b == bucket).count()
}

/// JSON views for scripts.
pub fn views(store: &CheckStore, now: DateTime<Utc>) -> Vec<CheckView> {
    let mut views = Vec::new();
    for check in store.open() {
        if let Some(bucket) = bucket_for(check, now) {
            views.push(CheckView {
                check: check.clone(),
                bucket: bucket.as_str().to_string(),
                due_in_seconds: check
                    .due_at
                    .map(|due| due.signed_duration_since(now).num_seconds()),
            });
        }
    }
    views.sort_by_key(|v| {
        let bucket = match v.bucket.as_str() {
            "overdue" => 0,
            "now" => 1,
            "today" => 2,
            "soon" => 3,
            "later" => 4,
            _ => 5,
        };
        (bucket, std::cmp::Reverse(v.check.priority.rank()))
    });
    views
}

/// Counts of newly created and refreshed checks from an import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub created: usize,
    pub updated: usize,
}

impl ImportOutcome {
    pub fn total(&self) -> usize {
        self.created + self.updated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub ok: bool,
    pub observed: String,
}

/// Run a restricted, read-only probe on the VPS.
///
/// Supported forms:
/// * `systemctl is-active <unit>`
/// * `disk_free > 5G`
/// * `http 200 https://example.com/health`
pub fn run_probe(
    runner: &dyn CommandRunner,
    cfg: &crate::config::Config,
    probe: &str,
) -> Result<ProbeResult> {
    let fields: Vec<&str> = probe.split_whitespace().collect();
    let ssh = |command: &str| {
        CommandSpec::new(&cfg.ssh_bin)
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=6"])
            .arg(&cfg.ssh_alias)
            .arg(command)
            .timeout(25)
    };
    match fields.as_slice() {
        ["systemctl", "is-active", unit] => {
            if !unit
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.@:-".contains(c))
            {
                bail!("invalid systemd unit in probe");
            }
            let output = runner.run(&ssh(&format!("systemctl is-active {unit}")))?;
            if !output.success() {
                return Ok(ProbeResult {
                    ok: false,
                    observed: output.stderr.trim().to_string(),
                });
            }
            let observed = output.stdout.trim().to_string();
            Ok(ProbeResult {
                ok: observed == "active",
                observed,
            })
        }
        ["disk_free", ">", size] => {
            let threshold_mb = parse_size_mb(size)?;
            let output = runner.run(&ssh("df -Pm / | awk 'NR==2{print $4}'"))?;
            if !output.success() {
                return Ok(ProbeResult {
                    ok: false,
                    observed: output.stderr.trim().to_string(),
                });
            }
            let free_mb: u64 = output
                .stdout
                .trim()
                .parse()
                .with_context(|| format!("unexpected df output: {}", output.stdout.trim()))?;
            Ok(ProbeResult {
                ok: free_mb > threshold_mb,
                observed: format!("{free_mb} MiB free (threshold {threshold_mb} MiB)"),
            })
        }
        ["http", code, url] => {
            let expected: u16 = code.parse().context("invalid HTTP status in probe")?;
            if !(100..=599).contains(&expected) {
                bail!("HTTP status must be between 100 and 599");
            }
            if !(url.starts_with("http://") || url.starts_with("https://"))
                || url.contains(['\'', '"', ' '])
            {
                bail!("invalid URL in probe");
            }
            let output = runner.run(&ssh(&format!(
                "curl -sS -o /dev/null -w '%{{http_code}}' --max-time 10 '{url}'"
            )))?;
            if !output.success() {
                return Ok(ProbeResult {
                    ok: false,
                    observed: output.stderr.trim().to_string(),
                });
            }
            let observed = output.stdout.trim().to_string();
            Ok(ProbeResult {
                ok: observed == expected.to_string(),
                observed,
            })
        }
        _ => bail!(
            "unsupported probe `{probe}` (use `systemctl is-active <unit>`, `disk_free > 5G` or `http 200 <url>`)"
        ),
    }
}

fn parse_size_mb(input: &str) -> Result<u64> {
    let text = input.trim().to_ascii_uppercase();
    let (number, multiplier) = match text.chars().last() {
        Some('G') => (&text[..text.len() - 1], 1024),
        Some('M') => (&text[..text.len() - 1], 1),
        Some('T') => (&text[..text.len() - 1], 1024 * 1024),
        _ => bail!("size must end with M, G or T"),
    };
    let value: f64 = number.parse().context("invalid size")?;
    Ok((value * multiplier as f64) as u64)
}

/// Import the `CHECKS.md` format written by agents:
///
/// ```text
/// - [ ] Verify the fetcher cron started | due: +2h | kind: vps | priority: high | detail: check logs
/// ```
///
/// Unknown fields are ignored, missing fields get defaults, and importing the
/// same line twice is a no-op (or refreshes the open entry).
pub fn import_markdown(
    store: &mut CheckStore,
    slug: &str,
    content: &str,
    now: DateTime<Utc>,
) -> ImportOutcome {
    let mut outcome = ImportOutcome::default();
    for line in content.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("- [ ] ")
            .or_else(|| trimmed.strip_prefix("* [ ] "))
        else {
            continue;
        };
        let mut parts = rest.split('|');
        let title = parts.next().unwrap_or("").trim();
        if title.is_empty() {
            continue;
        }
        let mut due_text = String::new();
        let mut kind = CheckKind::Manual;
        let mut priority = Priority::Normal;
        let mut detail = String::new();
        let mut every = None;
        let mut probe = None;
        for part in parts {
            let Some((key, value)) = part.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "due" | "when" => due_text = value.to_string(),
                "kind" | "type" => {
                    if let Ok(parsed) = value.parse() {
                        kind = parsed;
                    }
                }
                "priority" | "prio" => {
                    if let Ok(parsed) = value.parse() {
                        priority = parsed;
                    }
                }
                "detail" | "note" => detail = value.to_string(),
                "every" => every = parse_span(value).ok(),
                "probe" => probe = Some(value.to_string()),
                _ => {}
            }
        }
        let due_at = parse_due(&due_text, now).unwrap_or(None);

        if let Some(existing) = store
            .checks
            .iter_mut()
            .find(|c| c.slug == slug && c.title.eq_ignore_ascii_case(title))
        {
            if existing.status == CheckStatus::Open {
                existing.kind = kind;
                existing.priority = priority;
                existing.due_at = due_at;
                existing.every_secs = every;
                existing.probe = probe;
                if !detail.is_empty() {
                    existing.detail = detail;
                }
                existing.updated_at = now;
                outcome.updated += 1;
            }
            // Done checks are not reopened by a stale file.
            continue;
        }

        let mut check = Check::new(slug, title, kind, priority, due_at, detail, now);
        check.every_secs = every;
        check.probe = probe;
        store.add(check);
        outcome.created += 1;
    }
    outcome
}

/// Import every project's `CHECKS.md` into the store.
pub fn sync_projects(
    store: &mut CheckStore,
    projects_root: &Path,
    now: DateTime<Utc>,
) -> Result<ImportOutcome> {
    let mut outcome = ImportOutcome::default();
    for project in crate::resources::project_dirs(projects_root) {
        let slug = project
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let path = project.join("CHECKS.md");
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let partial = import_markdown(store, &slug, &content, now);
        outcome.created += partial.created;
        outcome.updated += partial.updated;
    }
    Ok(outcome)
}

/// Stable short identifier (FNV-1a, 8 hex chars).
pub fn short_id(seed: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        t("2026-10-20T12:00:00Z")
    }

    fn check(title: &str, due: Option<DateTime<Utc>>) -> Check {
        Check::new(
            "demo",
            title,
            CheckKind::Vps,
            Priority::Normal,
            due,
            "",
            now(),
        )
    }

    #[test]
    fn parses_relative_and_absolute_due_dates() {
        assert_eq!(
            parse_due("+2h", now()).unwrap(),
            Some(t("2026-10-20T14:00:00Z"))
        );
        assert_eq!(
            parse_due("90m", now()).unwrap(),
            Some(t("2026-10-20T13:30:00Z"))
        );
        assert_eq!(
            parse_due("in 3d", now()).unwrap(),
            Some(t("2026-10-23T12:00:00Z"))
        );
        assert_eq!(
            parse_due("1w", now()).unwrap(),
            Some(t("2026-10-27T12:00:00Z"))
        );
        assert_eq!(parse_due("now", now()).unwrap(), Some(now()));
        assert_eq!(
            parse_due("2026-11-01T09:30:00Z", now()).unwrap(),
            Some(t("2026-11-01T09:30:00Z"))
        );
        assert_eq!(parse_due("", now()).unwrap(), None);
        assert_eq!(parse_due("none", now()).unwrap(), None);
    }

    #[test]
    fn rejects_bad_due_dates() {
        assert!(parse_due("2", now()).is_err());
        assert!(parse_due("+0h", now()).is_err());
        assert!(parse_due("-3h", now()).is_err());
        assert!(parse_due("2 parsecs", now()).is_err());
        assert!(parse_due("soonish", now()).is_err());
    }

    #[test]
    fn buckets_follow_time_and_priority() {
        let n = now();
        assert_eq!(
            bucket_for(&check("late", Some(n - Duration::minutes(5))), n),
            Some(Bucket::Overdue)
        );
        assert_eq!(
            bucket_for(&check("soon", Some(n + Duration::hours(2))), n),
            Some(Bucket::Now)
        );
        assert_eq!(
            bucket_for(&check("today", Some(n + Duration::hours(8))), n),
            Some(Bucket::Today)
        );
        assert_eq!(
            bucket_for(&check("week", Some(n + Duration::days(3))), n),
            Some(Bucket::Soon)
        );
        assert_eq!(
            bucket_for(&check("month", Some(n + Duration::days(30))), n),
            Some(Bucket::Later)
        );
        assert_eq!(
            bucket_for(&check("none", None), n),
            Some(Bucket::Unscheduled)
        );

        let mut urgent = check("urgent", None);
        urgent.priority = Priority::Critical;
        assert_eq!(bucket_for(&urgent, n), Some(Bucket::Now));

        let mut done = check("done", None);
        done.status = CheckStatus::Done;
        assert_eq!(bucket_for(&done, n), None);
    }

    #[test]
    fn ordering_is_overdue_then_priority() {
        let mut store = CheckStore::default();
        let mut high = check("high unscheduled", None);
        high.priority = Priority::High;
        store.checks.push(high);
        store
            .checks
            .push(check("late normal", Some(now() - Duration::minutes(1))));
        let mut low_late = check("late low", Some(now() - Duration::minutes(2)));
        low_late.priority = Priority::Low;
        store.checks.push(low_late);
        store
            .checks
            .push(check("tomorrow", Some(now() + Duration::hours(20))));

        let ordered = ordered_open(&store, now());
        let titles: Vec<&str> = ordered.iter().map(|(c, _)| c.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["late normal", "late low", "high unscheduled", "tomorrow"]
        );
        assert_eq!(ordered[0].1, Bucket::Overdue);
        assert_eq!(ordered[2].1, Bucket::Now);
    }

    #[test]
    fn add_refreshes_open_duplicates_and_dedupes_ids() {
        let mut store = CheckStore::default();
        let first = check("same", None);
        let id = store.add(first.clone());
        let mut refreshed = first;
        refreshed.due_at = Some(now() + Duration::hours(1));
        let id2 = store.add(refreshed);
        assert_eq!(id, id2);
        assert_eq!(store.checks.len(), 1);
        assert_eq!(store.checks[0].due_at, Some(now() + Duration::hours(1)));
    }

    #[test]
    fn ids_are_stable_and_collisions_get_suffixes() {
        assert_eq!(short_id("demo|verify"), short_id("demo|verify"));
        assert_ne!(short_id("a"), short_id("b"));
        let mut store = CheckStore::default();
        let mut a = check("same", None);
        a.id = "fixed".into();
        store.add(a);
        let mut b = check("other", None);
        b.id = "fixed".into();
        let id = store.add(b);
        assert_eq!(id, "fixed-2");
    }

    #[test]
    fn done_and_reschedule() {
        let mut store = CheckStore::default();
        let id = store.add(check("verify", Some(now())));
        store
            .mark_done(&id, "looked good", now() + Duration::minutes(5))
            .unwrap();
        let done = store.get(&id).unwrap();
        assert_eq!(done.status, CheckStatus::Done);
        assert_eq!(done.done_note, "looked good");
        assert!(store.reschedule(&id, None, now()).is_err());
        assert!(store.mark_done("nope", "", now()).is_err());
    }

    #[test]
    fn relative_labels() {
        let n = now();
        assert_eq!(
            relative_label(&check("x", Some(n - Duration::minutes(12))), n),
            "overdue 12m"
        );
        assert_eq!(
            relative_label(&check("x", Some(n + Duration::minutes(30))), n),
            "in 30m"
        );
        assert_eq!(
            relative_label(&check("x", Some(n + Duration::hours(3))), n),
            "in 3h"
        );
        assert_eq!(
            relative_label(&check("x", Some(n + Duration::days(2))), n),
            "in 2d"
        );
        assert_eq!(
            relative_label(&check("x", Some(n + Duration::weeks(3))), n),
            "in 3w"
        );
        assert_eq!(relative_label(&check("x", None), n), "unscheduled");
    }

    #[test]
    fn imports_agent_markdown() {
        let mut store = CheckStore::default();
        let content = r#"
# Human checks

- [ ] Verify the fetcher cron started | due: +2h | kind: vps | priority: high | detail: check systemctl and logs
- [ ] Configure the API key | due: +1d | kind: api | priority: critical
- [x] already done, ignore me
not a checklist line
- [ ]   | due: +1h
"#;
        let outcome = import_markdown(&mut store, "demo", content, now());
        assert_eq!(outcome.created, 2);
        assert_eq!(outcome.updated, 0);
        assert_eq!(store.checks.len(), 2);
        let cron = store
            .checks
            .iter()
            .find(|c| c.title.contains("fetcher"))
            .unwrap();
        assert_eq!(cron.kind, CheckKind::Vps);
        assert_eq!(cron.priority, Priority::High);
        assert_eq!(cron.due_at, Some(t("2026-10-20T14:00:00Z")));
        assert!(cron.detail.contains("systemctl"));
    }

    #[test]
    fn import_is_idempotent_and_refreshes() {
        let mut store = CheckStore::default();
        let line = "- [ ] Verify cron | due: +2h | kind: vps";
        let first = import_markdown(&mut store, "demo", line, now());
        assert_eq!(first.created, 1);
        let second = import_markdown(&mut store, "demo", line, now());
        assert_eq!(second.created, 0);
        assert_eq!(second.updated, 1);

        let updated = "- [ ] Verify cron | due: +5h | kind: vps | priority: critical";
        let third = import_markdown(&mut store, "demo", updated, now());
        assert_eq!(third.updated, 1);
        assert_eq!(store.checks.len(), 1);
        assert_eq!(store.checks[0].priority, Priority::Critical);
        assert_eq!(store.checks[0].due_at, Some(t("2026-10-20T17:00:00Z")));

        // A done check is never reopened by a stale file.
        let id = store.checks[0].id.clone();
        store.mark_done(&id, "", now()).unwrap();
        let fourth = import_markdown(&mut store, "demo", updated, now());
        assert_eq!(fourth.created, 0);
        assert_eq!(fourth.updated, 0);
        assert_eq!(store.checks[0].status, CheckStatus::Done);
    }

    #[test]
    fn render_groups_by_bucket_and_hides_done() {
        let mut store = CheckStore::default();
        store.add(check("Verify cron", Some(now() - Duration::minutes(3))));
        store.add(check("Check API quota", Some(now() + Duration::days(2))));
        store.add(check("Read the logs", None));
        let id = store.add(check("Finished", None));
        store.mark_done(&id, "", now()).unwrap();

        let text = render(&store, now(), false);
        assert!(text.contains("OVERDUE (1)"));
        assert!(text.contains("SOON (1)"));
        assert!(text.contains("UNSCHEDULED (1)"));
        assert!(text.contains("Verify cron"));
        assert!(!text.contains("Finished"));
        assert!(text.contains("mox check done <id>"));
        assert!(text.contains("CHECKS"));

        let with_done = render(&store, now(), true);
        assert!(with_done.contains("DONE (1)"));
        assert!(with_done.contains("Finished"));
    }

    #[test]
    fn render_empty_store() {
        let text = render(&CheckStore::default(), now(), false);
        assert!(text.contains("nothing to check"));
    }

    #[test]
    fn oneline_summarises_counts() {
        let mut store = CheckStore::default();
        assert_eq!(oneline(&store, now()), "checks: nothing to check");
        store.add(check("late", Some(now() - Duration::minutes(1))));
        store.add(check("later", Some(now() + Duration::days(3))));
        assert_eq!(oneline(&store, now()), "checks: 1 overdue · 1 soon");
    }

    #[test]
    fn views_include_bucket_and_countdown() {
        let mut store = CheckStore::default();
        store.add(check("late", Some(now() - Duration::minutes(2))));
        let views = views(&store, now());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].bucket, "overdue");
        assert_eq!(views[0].due_in_seconds, Some(-120));
    }

    #[test]
    fn recurrence_reschedules_instead_of_closing() {
        let mut store = CheckStore::default();
        let mut check = check("daily audit", Some(now()));
        check.every_secs = Some(86_400);
        let id = store.add(check);
        store.mark_done(&id, "ok", now()).unwrap();
        let stored = store.get(&id).unwrap();
        assert_eq!(stored.status, CheckStatus::Open);
        assert_eq!(stored.due_at, Some(now() + Duration::days(1)));
        assert!(stored.done_at.is_some());
    }

    #[test]
    fn imports_every_and_probe_fields() {
        let mut store = CheckStore::default();
        import_markdown(
            &mut store,
            "demo",
            "- [ ] Watch the cron | due: +1h | every: 1d | probe: systemctl is-active fetcher\n",
            now(),
        );
        let check = &store.checks[0];
        assert_eq!(check.every_secs, Some(86_400));
        assert_eq!(check.probe.as_deref(), Some("systemctl is-active fetcher"));
    }

    #[test]
    fn parse_span_units() {
        assert_eq!(parse_span("30m").unwrap(), 1800);
        assert_eq!(parse_span("2h").unwrap(), 7200);
        assert_eq!(parse_span("1d").unwrap(), 86_400);
        assert_eq!(parse_span("1w").unwrap(), 604_800);
        assert!(parse_span("0h").is_err());
        assert!(parse_span("soon").is_err());
    }

    #[test]
    fn probes_are_read_only_and_restricted() {
        use crate::exec::FakeRunner;
        use crate::exec::Output;
        let cfg = crate::config::Config::default();
        let runner = FakeRunner::new(|spec| {
            let command = spec.args.last().cloned().unwrap_or_default();
            if command.contains("systemctl is-active") {
                return Output {
                    code: Some(0),
                    stdout: "active\n".into(),
                    ..Output::default()
                };
            }
            if command.contains("df -Pm") {
                return Output {
                    code: Some(0),
                    stdout: "4096\n".into(),
                    ..Output::default()
                };
            }
            if command.contains("curl") {
                return Output {
                    code: Some(0),
                    stdout: "200\n".into(),
                    ..Output::default()
                };
            }
            Output::ok()
        });
        assert!(
            run_probe(&runner, &cfg, "systemctl is-active fetcher")
                .unwrap()
                .ok
        );
        assert!(run_probe(&runner, &cfg, "disk_free > 1G").unwrap().ok);
        assert!(!run_probe(&runner, &cfg, "disk_free > 8G").unwrap().ok);
        assert!(
            run_probe(&runner, &cfg, "http 200 https://example.com/health")
                .unwrap()
                .ok
        );
        assert!(
            !run_probe(&runner, &cfg, "http 500 https://example.com/health")
                .unwrap()
                .ok
        );
        assert!(run_probe(&runner, &cfg, "rm -rf /").is_err());
        assert!(run_probe(&runner, &cfg, "systemctl is-active bad;unit").is_err());
        assert!(run_probe(&runner, &cfg, "http 200 file:///etc/passwd").is_err());
    }

    #[test]
    fn remove_reopen_and_limit() {
        let mut store = CheckStore::default();
        let id = store.add(check("first", Some(now() - Duration::minutes(1))));
        store.add(check("second", Some(now() + Duration::hours(1))));
        store.add(check("third", Some(now() + Duration::hours(2))));
        store.mark_done(&id, "ok", now()).unwrap();
        store.reopen(&id, now()).unwrap();
        assert_eq!(store.get(&id).unwrap().status, CheckStatus::Open);
        assert!(store.get(&id).unwrap().done_note.is_empty());
        let text = render_limited(&store, now(), false, Some(1));
        assert!(text.contains("first"), "{text}");
        assert!(!text.contains("second"), "{text}");
        store.remove(&id).unwrap();
        assert!(store.get(&id).is_none());
        assert!(store.remove("nope").is_err());
    }

    #[test]
    fn store_roundtrip_and_atomic_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checks.json");
        let mut store = load(&path).unwrap();
        assert!(store.checks.is_empty());
        store.add(check("x", None));
        save(&path, &store).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.checks.len(), 1);
        assert_eq!(loaded.checks[0].title, "x");
        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files, vec!["checks.json"]);
    }

    #[test]
    fn kind_and_priority_parsing() {
        assert_eq!(CheckKind::from_str("VPS").unwrap(), CheckKind::Vps);
        assert_eq!(CheckKind::from_str("api").unwrap(), CheckKind::Api);
        assert!(CheckKind::from_str("nope").is_err());
        assert_eq!(Priority::from_str("URGENT").unwrap(), Priority::Critical);
        assert!(Priority::from_str("nope").is_err());
    }

    #[test]
    fn syncs_from_project_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::write(
            root.join("alpha/CHECKS.md"),
            "- [ ] Alpha check | due: +1h\n",
        )
        .unwrap();
        std::fs::write(root.join("beta/CHECKS.md"), "- [ ] Beta check\n").unwrap();
        std::fs::write(root.join("beta/README.md"), "ignored").unwrap();

        let mut store = CheckStore::default();
        let outcome = sync_projects(&mut store, root, now()).unwrap();
        assert_eq!(outcome.created, 2);
        assert!(store.checks.iter().any(|c| c.slug == "alpha"));
        assert!(store.checks.iter().any(|c| c.slug == "beta"));
        assert_eq!(store.checks.len(), 2);
    }
}
