//! Read-only Telegram bot for mox.
//!
//! `mox bot` long-polls `getUpdates` and answers commands/presses with the same
//! data `mox status`, `mox milestone`, `mox check`, `mox stats` and `mox report`
//! show locally. It is read-only: no command can mutate a project, start an
//! agent or answer a question. Chat id allowlisting is enforced.
//!
//! The bot token never appears on a command line: Telegram requests go through a
//! 0600 curl config file, and message bodies through a 0600 JSON file.

use crate::checks;
use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec};
use crate::milestones;
use crate::stats;
use crate::util::{display_path, tail_lines, truncate_chars};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_MESSAGE: usize = 3800;

/// True when this process looks like it runs on a remote/VPS checkout.
/// `mox bot` refuses in this context unless a human explicitly allows it.
pub fn remote_context(cwd: &Path) -> bool {
    if std::env::var("MOX_REMOTE_RUN").is_ok() {
        return true;
    }
    cwd.join(".mox/remote.owner").exists() || cwd.to_string_lossy().contains("/mox-remote/")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub text: String,
    pub markup: Option<Value>,
}

impl Reply {
    fn text(text: impl Into<String>) -> Self {
        Reply {
            text: text.into(),
            markup: None,
        }
    }

    fn with_markup(mut self, markup: Value) -> Self {
        self.markup = Some(markup);
        self
    }
}

pub struct Bot {
    cfg: Config,
    runner: Arc<dyn CommandRunner>,
    token: String,
    offset: i64,
}

impl Bot {
    pub fn new(cfg: &Config, runner: Arc<dyn CommandRunner>) -> Result<Self> {
        if cfg.telegram_chat_id.trim().is_empty() {
            bail!("set `telegram_chat_id` in the mox config before running `mox bot`");
        }
        let token = std::env::var(&cfg.telegram_token_env).unwrap_or_default();
        if token.trim().is_empty() {
            bail!(
                "telegram bot enabled but ${} is not set",
                cfg.telegram_token_env
            );
        }
        let offset = read_offset(cfg).unwrap_or(0);
        Ok(Bot {
            cfg: cfg.clone(),
            runner,
            token: token.trim().to_string(),
            offset,
        })
    }

    /// Process one long-poll batch. Returns the number of authorized updates.
    pub fn poll_once(&mut self) -> Result<usize> {
        let url = format!(
            "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout=30&allowed_updates=%5B%22message%22%2C%22callback_query%22%5D",
            self.token, self.offset
        );
        let value = self.api_url(&url, None, 40)?;
        let updates = value
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut handled = 0usize;
        for update in &updates {
            let update_id = update.get("update_id").and_then(|v| v.as_i64());
            if let Some(id) = update_id {
                self.offset = self.offset.max(id + 1);
            }
            if let Some(message) = update.get("message") {
                let chat_id = message
                    .get("chat")
                    .and_then(|c| c.get("id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default();
                if !self.authorized(chat_id) {
                    continue;
                }
                let text = message
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let reply = self.handle_text(text);
                self.send(chat_id, &reply)?;
                handled += 1;
            }
            if let Some(callback) = update.get("callback_query") {
                let chat_id = callback
                    .get("message")
                    .and_then(|m| m.get("chat"))
                    .and_then(|c| c.get("id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default();
                let callback_id = callback.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !self.authorized(chat_id) {
                    continue;
                }
                let data = callback.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let reply = self.handle_callback(data);
                self.answer_callback(callback_id)?;
                self.send(chat_id, &reply)?;
                handled += 1;
            }
        }
        if !updates.is_empty() {
            write_offset(&self.cfg, self.offset);
        }
        Ok(handled)
    }

    pub fn run(&mut self, once: bool) -> Result<()> {
        loop {
            match self.poll_once() {
                Ok(n) => {
                    if once {
                        println!("processed {n} update(s)");
                        return Ok(());
                    }
                }
                Err(err) => {
                    eprintln!("telegram poll failed: {err:#}");
                    if once {
                        return Err(err);
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
            if crate::shutdown::requested() {
                return Ok(());
            }
        }
    }

    fn authorized(&self, chat_id: i64) -> bool {
        chat_id.to_string() == self.cfg.telegram_chat_id.trim()
    }

    // ------------------------------------------------------------------ routing

    pub fn handle_text(&self, text: &str) -> Reply {
        let text = text.trim();
        if text.is_empty() {
            return Reply::text("mox bot — send /projects or /help");
        }
        let mut parts = text.split_whitespace();
        let command = parts
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let args: Vec<&str> = parts.collect();
        match command.as_str() {
            "/start" | "/help" => Reply::text(help_text()),
            "/projects" | "/p" if args.is_empty() => self.projects(),
            "/project" | "/p" => {
                if let Some(slug) = args.first() {
                    self.project_page(slug)
                } else {
                    self.projects()
                }
            }
            "/status" | "/s" => self.status_page(),
            "/milestones" | "/m" => self.milestones_page(args.first().copied()),
            "/todos" | "/t" => self.todos_page(args.first().copied()),
            "/checks" | "/c" => self.checks_page(),
            "/questions" | "/q" => self.questions_page(),
            "/stats" => Reply::text(stats::render(&stats::summarize(
                &stats::load(&self.cfg.stats_file()).unwrap_or_default(),
            ))),
            "/report" | "/r" => self.report_page(),
            "/logs" | "/l" => self.logs_page(args.first().copied(), args.get(1).copied()),
            _ => Reply::text(help_text()),
        }
    }

    pub fn handle_callback(&self, data: &str) -> Reply {
        let (kind, arg) = data.split_once(':').unwrap_or((data, ""));
        match kind {
            "p" => self.project_page(arg),
            "l" => self.logs_page(Some(arg), None),
            "m" => self.milestones_page(Some(arg)),
            "t" => self.todos_page(Some(arg)),
            "c" => self.checks_page(),
            "s" => Reply::text(stats::render(&stats::summarize(
                &stats::load(&self.cfg.stats_file()).unwrap_or_default(),
            ))),
            "r" => self.report_page(),
            "b" => self.projects(),
            _ => Reply::text(help_text()),
        }
    }

    // -------------------------------------------------------------------- views

    fn projects(&self) -> Reply {
        let state = crate::state::load(&self.cfg.state_file()).unwrap_or_default();
        if state.projects.is_empty() {
            return Reply::text("No projects yet. Run `mox plan` then `mox up`.");
        }
        let now = Utc::now();
        let mut lines = vec!["mox projects".to_string()];
        let mut rows: Vec<Value> = Vec::new();
        for project in state.ordered() {
            let snapshot = self.snapshot(project, now);
            lines.push(format!(
                "• {} — {}{} · {}",
                project.slug,
                project.status.as_str(),
                if project.target.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", project.target)
                },
                snapshot.progress
            ));
            rows.push(json!([{
                "text": format!("{} · {}", project.slug, project.status.as_str()),
                "callback_data": format!("p:{}", project.slug)
            }]));
        }
        lines.push("\nUse the buttons or /project <slug>.".to_string());
        Reply::text(lines.join("\n")).with_markup(json!({ "inline_keyboard": rows }))
    }

    fn project_page(&self, slug: &str) -> Reply {
        let state = crate::state::load(&self.cfg.state_file()).unwrap_or_default();
        let Some(project) = state.project(slug) else {
            return Reply::text(format!("Unknown project `{slug}`. Try /projects."));
        };
        let now = Utc::now();
        let dir = PathBuf::from(&project.path);
        let mut lines = vec![
            format!("{} — {}", project.slug, project.status.as_str()),
            format!(
                "target: {} · attempts: {} · repo: {}",
                if project.target.is_empty() {
                    "local"
                } else {
                    &project.target
                },
                project.attempts,
                if project.repo_url.is_empty() {
                    "-"
                } else {
                    &project.repo_url
                }
            ),
        ];
        if let Some(board) = milestones::load_optional(&milestones::board_path(&dir)) {
            let p = milestones::progress(&board, now);
            lines.push(format!(
                "milestones: {}/{} · {}% · ETA {}",
                p.done,
                p.total,
                p.percent,
                p.compact_eta()
            ));
            if let Some(current) = &p.current {
                if let Some(milestone) = board.milestones.iter().find(|m| &m.id == current) {
                    lines.push(format!(
                        "current: {} {} [{}] ~{}",
                        milestone.id,
                        milestone.title,
                        milestone.complexity.as_str(),
                        crate::util::format_secs(milestone.estimate_secs)
                    ));
                    let open = milestone.open_todos();
                    if open > 0 {
                        lines.push(format!("todos left in current: {open}"));
                    }
                }
            }
            for milestone in board
                .milestones
                .iter()
                .filter(|m| matches!(m.status, milestones::MilestoneStatus::Blocked))
            {
                lines.push(format!(
                    "blocked: {} {}{}",
                    milestone.id,
                    milestone.title,
                    if milestone.note.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", milestone.note)
                    }
                ));
            }
        } else {
            lines.push("milestones: no board".to_string());
        }
        let check_count = checks::load(&self.cfg.checks_file())
            .map(|store| store.open().iter().filter(|c| c.slug == slug).count())
            .unwrap_or(0);
        if check_count > 0 {
            lines.push(format!("human checks: {check_count} open (/checks)"));
        }
        if let Some(error) = &project.last_error {
            lines.push(format!("last error: {}", truncate_chars(error, 300)));
        }
        if let Ok(status) = std::fs::read_to_string(dir.join("STATUS.md")) {
            let excerpt: Vec<&str> = status
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(6)
                .collect();
            if !excerpt.is_empty() {
                lines.push("STATUS.md:".to_string());
                lines.extend(excerpt.into_iter().map(|l| format!("  {l}")));
            }
        }
        let markup = json!({
            "inline_keyboard": [
                [
                    {"text": "Milestones", "callback_data": format!("m:{slug}")},
                    {"text": "Todos", "callback_data": format!("t:{slug}")}
                ],
                [
                    {"text": "Checks", "callback_data": "c"},
                    {"text": "Logs", "callback_data": format!("l:{slug}")}
                ],
                [
                    {"text": "All projects", "callback_data": "b"},
                    {"text": "Report", "callback_data": "r"}
                ]
            ]
        });
        Reply::text(lines.join("\n")).with_markup(markup)
    }

    fn status_page(&self) -> Reply {
        let state = crate::state::load(&self.cfg.state_file()).unwrap_or_default();
        if state.projects.is_empty() {
            return Reply::text("No projects yet.");
        }
        let now = Utc::now();
        let mut lines = vec!["mox status".to_string()];
        for project in state.ordered() {
            let s = self.snapshot(project, now);
            let checks = checks::load(&self.cfg.checks_file())
                .map(|store| {
                    store
                        .open()
                        .iter()
                        .filter(|c| {
                            matches!(
                                checks::bucket_for(c, now),
                                Some(checks::Bucket::Overdue | checks::Bucket::Now)
                            ) && c.slug == project.slug
                        })
                        .count()
                })
                .unwrap_or(0);
            lines.push(format!(
                "• {} — {}{} · {} · ETA {} · checks {checks}",
                project.slug,
                project.status.as_str(),
                if project.target == "vps" && project.remote_pid.is_some() {
                    " (detached)"
                } else {
                    ""
                },
                s.progress,
                s.eta
            ));
        }
        Reply::text(lines.join("\n"))
    }

    fn milestones_page(&self, slug: Option<&str>) -> Reply {
        let state = crate::state::load(&self.cfg.state_file()).unwrap_or_default();
        let projects: Vec<_> = match slug {
            Some(slug) => state.project(slug).into_iter().collect(),
            None => state.ordered(),
        };
        if projects.is_empty() {
            return Reply::text("No matching project.");
        }
        let mut out = String::new();
        for project in projects {
            let path = milestones::board_path(Path::new(&project.path));
            match milestones::load_optional(&path) {
                Some(board) => {
                    out.push_str(&format!(
                        "{} — {}\n",
                        project.slug,
                        milestones::render_progress(&board, Utc::now())
                    ));
                }
                None => out.push_str(&format!("{} — no MILESTONES.json\n", project.slug)),
            }
        }
        Reply::text(truncate_chars(&out, MAX_MESSAGE))
    }

    fn todos_page(&self, slug: Option<&str>) -> Reply {
        let state = crate::state::load(&self.cfg.state_file()).unwrap_or_default();
        let projects: Vec<_> = match slug {
            Some(slug) => state.project(slug).into_iter().collect(),
            None => state.ordered(),
        };
        let mut lines = vec!["Open todos".to_string()];
        for project in projects {
            let board =
                milestones::load_optional(&milestones::board_path(Path::new(&project.path)));
            let Some(board) = board else {
                continue;
            };
            let open: Vec<_> = board
                .milestones
                .iter()
                .filter(|m| m.status == milestones::MilestoneStatus::InProgress)
                .chain(
                    board
                        .milestones
                        .iter()
                        .filter(|m| m.status == milestones::MilestoneStatus::Pending),
                )
                .flat_map(|m| {
                    m.todos
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| !t.done)
                        .map(move |(i, t)| (m, i, t))
                })
                .collect();
            if open.is_empty() {
                continue;
            }
            lines.push(format!("\n{}:", project.slug));
            for (milestone, index, todo) in open.into_iter().take(12) {
                lines.push(format!(
                    "  {} #{}. [ ] {}",
                    milestone.id,
                    index + 1,
                    todo.text
                ));
            }
        }
        if lines.len() == 1 {
            lines.push("Nothing open.".to_string());
        }
        Reply::text(lines.join("\n"))
    }

    fn checks_page(&self) -> Reply {
        let store = checks::load(&self.cfg.checks_file()).unwrap_or_default();
        let now = Utc::now();
        let mut text = checks::render_limited(&store, now, false, Some(10));
        let questions = self.questions_text();
        if !questions.is_empty() {
            text.push_str("\n\n");
            text.push_str(&questions);
        }
        Reply::text(truncate_chars(&text, MAX_MESSAGE))
    }

    fn questions_page(&self) -> Reply {
        let questions = self.questions_text();
        Reply::text(if questions.is_empty() {
            "No open questions or checks.".to_string()
        } else {
            questions
        })
    }

    fn questions_text(&self) -> String {
        let mut lines = Vec::new();
        if let Ok(Some(plan)) = crate::state::load_plan(&self.cfg.plan_file()) {
            for spec in &plan.specs {
                let unanswered: Vec<_> = spec
                    .questions
                    .iter()
                    .filter(|q| {
                        spec.answers
                            .get(&q.id)
                            .map(|a| a.trim().is_empty())
                            .unwrap_or(true)
                    })
                    .collect();
                if !unanswered.is_empty() {
                    lines.push(format!(
                        "{}: {} question(s) waiting",
                        spec.slug,
                        unanswered.len()
                    ));
                }
            }
        }
        let store = checks::load(&self.cfg.checks_file()).unwrap_or_default();
        let now = Utc::now();
        for (check, bucket) in checks::ordered_open(&store, now)
            .into_iter()
            .filter(|(_, b)| matches!(b, checks::Bucket::Overdue | checks::Bucket::Now))
            .take(8)
        {
            lines.push(format!(
                "check: {} — {} ({})",
                check.slug,
                check.title,
                checks::relative_label(check, now)
            ));
            let _ = bucket;
        }
        if lines.is_empty() {
            String::new()
        } else {
            format!("Open questions / checks\n{}", lines.join("\n"))
        }
    }

    fn report_page(&self) -> Reply {
        let state = crate::state::load(&self.cfg.state_file()).unwrap_or_default();
        let now = Utc::now();
        let mut lines = vec![
            "mox report".to_string(),
            crate::host::HostProfile::detect().describe(),
            String::new(),
        ];
        for project in state.ordered() {
            let s = self.snapshot(project, now);
            lines.push(format!(
                "{} — {} · {} · ETA {}",
                project.slug,
                project.status.as_str(),
                s.progress,
                s.eta
            ));
        }
        let checks_store = checks::load(&self.cfg.checks_file()).unwrap_or_default();
        lines.push(String::new());
        lines.push(checks::oneline(&checks_store, now));
        Reply::text(truncate_chars(&lines.join("\n"), MAX_MESSAGE))
    }

    fn logs_page(&self, slug: Option<&str>, lines: Option<&str>) -> Reply {
        let Some(slug) = slug else {
            return Reply::text("Usage: /logs <slug> [lines]");
        };
        let n: usize = lines.and_then(|v| v.parse().ok()).unwrap_or(20).min(100);
        let local = self.cfg.logs_dir().join(format!("{slug}.log"));
        let remote = self.cfg.logs_dir().join(format!("{slug}.remote.log"));
        let path = if local.exists() { local } else { remote };
        match tail_lines(&path, n) {
            Ok(tail) if !tail.is_empty() => Reply::text(truncate_chars(
                &format!("{slug} ({})\n{}", display_path(&path), tail.join("\n")),
                MAX_MESSAGE,
            )),
            _ => Reply::text(format!("No log for `{slug}`.")),
        }
    }

    fn snapshot(
        &self,
        project: &crate::model::ProjectState,
        now: chrono::DateTime<Utc>,
    ) -> Snapshot {
        let board = milestones::load_optional(&milestones::board_path(Path::new(&project.path)));
        match board {
            Some(board) => {
                let p = milestones::progress(&board, now);
                Snapshot {
                    progress: p.short(),
                    eta: p.compact_eta(),
                }
            }
            None => Snapshot {
                progress: "-".to_string(),
                eta: "-".to_string(),
            },
        }
    }

    // ---------------------------------------------------------------------- api

    fn send(&self, chat_id: i64, reply: &Reply) -> Result<()> {
        let mut chunks = split_message(&reply.text);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        for (index, chunk) in chunks.iter().enumerate() {
            let mut body = json!({
                "chat_id": chat_id,
                "text": chunk,
                "disable_web_page_preview": true
            });
            if index == chunks.len() - 1 {
                if let Some(markup) = &reply.markup {
                    body["reply_markup"] = markup.clone();
                }
            }
            self.api_url(
                &format!("https://api.telegram.org/bot{}/sendMessage", self.token),
                Some(body),
                20,
            )?;
        }
        Ok(())
    }

    fn answer_callback(&self, callback_id: &str) -> Result<()> {
        if callback_id.is_empty() {
            return Ok(());
        }
        self.api_url(
            &format!(
                "https://api.telegram.org/bot{}/answerCallbackQuery",
                self.token
            ),
            Some(json!({"callback_query_id": callback_id})),
            15,
        )?;
        Ok(())
    }

    /// Shared curl call. URL (with token) and JSON bodies live in 0600 files.
    fn api_url(&self, url: &str, body: Option<Value>, timeout: u64) -> Result<Value> {
        let dir = self.cfg.state_dir_path().join("notify");
        crate::util::ensure_dir_private(&dir)?;
        let stamp = format!("{}-{}", std::process::id(), crate::util::timestamp_millis());
        let conf_path = dir.join(format!("bot-{stamp}.conf"));
        let body_path = dir.join(format!("bot-{stamp}.json"));
        let cleanup = TempCleanup(vec![conf_path.clone(), body_path.clone()]);

        let conf = format!("url = \"{}\"\n", curl_escape(url));
        crate::util::write_private(&conf_path, conf)?;
        let mut spec = CommandSpec::new("curl")
            .args(["-sS", "--max-time", &timeout.to_string(), "--config"])
            .arg(conf_path.display().to_string())
            .timeout(timeout + 10);
        if let Some(body) = body {
            crate::util::write_private(&body_path, serde_json::to_vec(&body)?)?;
            spec = spec
                .args([
                    "--header",
                    "Content-Type: application/json",
                    "--data-binary",
                ])
                .arg(format!("@{}", body_path.display()));
        }
        let output = self.runner.run(&spec)?;
        drop(cleanup);
        if !output.success() {
            bail!("telegram API call failed: {}", output.tail(4));
        }
        let value: Value =
            serde_json::from_str(&output.stdout).context("telegram returned invalid JSON")?;
        if value.get("ok") == Some(&Value::Bool(true)) {
            Ok(value)
        } else {
            bail!(
                "telegram rejected the request: {}",
                value
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            )
        }
    }
}

struct Snapshot {
    progress: String,
    eta: String,
}

struct TempCleanup(Vec<PathBuf>);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn curl_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

fn split_message(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_MESSAGE {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if current.chars().count() + line.chars().count() + 1 > MAX_MESSAGE {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

fn offset_path(cfg: &Config) -> PathBuf {
    cfg.state_dir_path().join("telegram.offset")
}

fn read_offset(cfg: &Config) -> Option<i64> {
    std::fs::read_to_string(offset_path(cfg))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_offset(cfg: &Config, offset: i64) {
    let _ = crate::util::write_private(&offset_path(cfg), offset.to_string());
}

fn help_text() -> String {
    [
        "mox bot (read-only)",
        "",
        "/projects — project list",
        "/project <slug> — details, ETA, current milestone, STATUS.md",
        "/status — one-line status for every project",
        "/milestones [slug] — milestone table",
        "/todos [slug] — open todos",
        "/checks — due human checks",
        "/questions — questions/checks waiting for you",
        "/stats — cross-project timing statistics",
        "/report — compact report",
        "/logs <slug> [n] — last log lines",
        "",
        "Buttons work too. No command mutates anything.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{FakeRunner, Output};
    use crate::model::{Idea, ProjectSpec, ProjectState, ProjectStatus};
    use crate::state::State;
    use std::sync::Mutex;

    fn cfg(root: &Path) -> Config {
        Config {
            projects_root: root.join("projects").display().to_string(),
            state_dir: root.join("state").display().to_string(),
            telegram_chat_id: "42".into(),
            telegram_token_env: "MOX_TEST_BOT_TOKEN".into(),
            ..Config::default()
        }
    }

    fn bot_with(root: &Path, updates: Value) -> Bot {
        std::env::set_var("MOX_TEST_BOT_TOKEN", "test-token");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_for_test = seen.clone();
        let runner = FakeRunner::new(move |spec| {
            seen_for_test.lock().unwrap().push(spec.args.join(" "));
            Output {
                code: Some(0),
                stdout: updates.to_string(),
                ..Output::default()
            }
        });
        Bot::new(&cfg(root), Arc::new(runner)).unwrap()
    }

    fn seed(root: &Path) {
        let cfg = cfg(root);
        let project_dir = cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project_dir).unwrap();
        let spec = ProjectSpec::from_idea(
            &Idea {
                title: Some("Demo".into()),
                details: "demo project".into(),
                raw: String::new(),
            },
            "demo".into(),
        );
        let mut spec = spec;
        spec.milestones = milestones::fallback_for(&spec);
        let mut board = milestones::board_from_spec(&spec, Utc::now());
        milestones::start(&mut board, "m1", Utc::now()).unwrap();
        milestones::done(
            &mut board,
            "m1",
            "",
            Utc::now() + chrono::Duration::minutes(5),
        )
        .unwrap();
        milestones::start(&mut board, "m2", Utc::now()).unwrap();
        milestones::save(&milestones::board_path(&project_dir), &board).unwrap();
        std::fs::write(
            project_dir.join("STATUS.md"),
            "Working: parser\nNext: tests\n",
        )
        .unwrap();
        let mut state = State::default();
        let mut project = ProjectState::new("demo", "Demo", &project_dir.display().to_string());
        project.status = ProjectStatus::Running;
        project.target = "local".into();
        state.upsert(project);
        crate::state::save(&cfg.state_file(), &state).unwrap();
        let mut checks = checks::CheckStore::default();
        checks.add(checks::Check::new(
            "demo",
            "Verify the fetcher",
            checks::CheckKind::Vps,
            checks::Priority::High,
            Some(Utc::now() - chrono::Duration::minutes(5)),
            "",
            Utc::now(),
        ));
        checks::save(&cfg.checks_file(), &checks).unwrap();
        let mut plan = crate::model::Plan::default();
        let mut spec2 = spec.clone();
        spec2.questions.push(crate::model::Question {
            id: "q1".into(),
            question: "Which DB?".into(),
            why: None,
            default: Some("SQLite".into()),
        });
        plan.specs = vec![spec2];
        crate::state::save_plan(&cfg.plan_file(), &plan).unwrap();
    }

    #[test]
    fn projects_and_project_pages_render_progress() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let bot = bot_with(dir.path(), json!({"ok": true, "result": []}));
        let projects = bot.handle_text("/projects");
        assert!(projects.text.contains("demo"), "{}", projects.text);
        assert!(projects.markup.is_some());
        let page = bot.handle_text("/project demo");
        assert!(page.text.contains("milestones: 1/3"), "{}", page.text);
        assert!(page.text.contains("ETA"), "{}", page.text);
        assert!(page.text.contains("Which DB?").eq(&false));
        assert!(page.text.contains("Working: parser"), "{}", page.text);
        let callback = bot.handle_callback("p:demo");
        assert!(callback.text.contains("demo"));
    }

    #[test]
    fn todos_checks_questions_and_stats() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let bot = bot_with(dir.path(), json!({"ok": true, "result": []}));
        let todos = bot.handle_text("/todos demo");
        assert!(todos.text.contains("[ ]"), "{}", todos.text);
        let checks_page = bot.handle_text("/checks");
        assert!(
            checks_page.text.contains("Verify the fetcher"),
            "{}",
            checks_page.text
        );
        let questions = bot.handle_text("/questions");
        assert!(
            questions.text.contains("Verify the fetcher"),
            "{}",
            questions.text
        );
        assert!(
            questions.text.contains("question(s) waiting"),
            "{}",
            questions.text
        );
        let stats = bot.handle_text("/stats");
        assert!(stats.text.contains("milestones"), "{}", stats.text);
    }

    #[test]
    fn unauthorized_chats_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let bot = bot_with(dir.path(), json!({"ok": true, "result": []}));
        assert!(!bot.authorized(999));
        assert!(bot.authorized(42));
    }

    #[test]
    fn poll_processes_messages_and_persists_offset() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let updates = json!({
            "ok": true,
            "result": [{
                "update_id": 10,
                "message": {"chat": {"id": 42}, "text": "/projects"}
            }]
        });
        let bot = bot_with(dir.path(), updates);
        let mut bot = bot;
        assert_eq!(bot.poll_once().unwrap(), 1);
        assert_eq!(bot.offset, 11);
        assert_eq!(read_offset(&cfg(dir.path())), Some(11));

        // Unauthorized chat is not handled.
        let updates = json!({
            "ok": true,
            "result": [{
                "update_id": 11,
                "message": {"chat": {"id": 7}, "text": "/projects"}
            }]
        });
        let mut bot = bot_with(dir.path(), updates);
        assert_eq!(bot.poll_once().unwrap(), 0);
    }

    #[test]
    fn split_keeps_messages_under_the_limit() {
        let text = "line\n".repeat(2000);
        let chunks = split_message(&text);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= MAX_MESSAGE));
    }

    #[test]
    fn detects_remote_context() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!remote_context(dir.path()));
        std::fs::create_dir_all(dir.path().join(".mox")).unwrap();
        std::fs::write(dir.path().join(".mox/remote.owner"), "demo\n").unwrap();
        assert!(remote_context(dir.path()));
        assert!(remote_context(Path::new("/home/u/mox-remote/demo")));
    }

    #[test]
    fn requires_token_and_chat_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg(dir.path());
        cfg.telegram_token_env = "MOX_TEST_BOT_TOKEN_MISSING".into();
        std::env::remove_var("MOX_TEST_BOT_TOKEN_MISSING");
        assert!(Bot::new(&cfg, Arc::new(FakeRunner::ok())).is_err());
        std::env::set_var("MOX_TEST_BOT_TOKEN_MISSING", "x");
        cfg.telegram_chat_id.clear();
        assert!(Bot::new(&cfg, Arc::new(FakeRunner::ok())).is_err());
        std::env::remove_var("MOX_TEST_BOT_TOKEN_MISSING");
    }
}
