//! CLI command implementations. Every external effect goes through
//! [`CommandRunner`] or the injected [`Brain`], so the whole workflow can run in
//! tests with no network, no GitHub and no OpenCode.

use crate::brain::Brain;
use crate::checks::{self, Check, CheckKind, CheckStore, Priority};
use crate::clean;
use crate::cli::{CheckAction, Command, NotifyAction, VpsCommand};
use crate::config::Config;
use crate::events;
use crate::exec::{CommandRunner, CommandSpec};
use crate::github;
use crate::ideas;
use crate::lock::LockGuard;
use crate::milestones;
use crate::model::{ComputeNeed, Plan, ProjectSpec, ProjectState, ProjectStatus};
use crate::notify::{self, Event, EventKind, Notifier};
use crate::prompts;
use crate::resources::{dir_size_bytes, DfProbe, DiskGuard, DiskProbe, PruneKind};
use crate::runner::{self, ProjectRun};
use crate::scaffold::scaffold_project;
use crate::scheduler::Scheduler;
use crate::state::{self, State};
use crate::stats;
use crate::util::{
    display_path, format_secs, format_tokens, human_mb, now_rfc3339, tail_lines, truncate_chars,
};
use crate::vps::{self, HostCapacity};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Asks the human clarifying questions.
pub trait Asker {
    fn ask(&mut self, prompt: &str, default: Option<&str>) -> Result<Option<String>>;
}

/// Reads answers from stdin.
pub struct StdinAsker;

impl Asker for StdinAsker {
    fn ask(&mut self, prompt: &str, default: Option<&str>) -> Result<Option<String>> {
        print!("{prompt}");
        if let Some(default) = default {
            print!(" [{default}]");
        }
        print!(": ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let answer = line.trim();
        Ok(if answer.is_empty() {
            None
        } else {
            Some(answer.to_string())
        })
    }
}

/// Deterministic asker for tests.
#[derive(Default)]
pub struct ScriptedAsker {
    answers: VecDeque<Option<String>>,
    pub prompts: Vec<String>,
}

impl ScriptedAsker {
    pub fn new(answers: impl IntoIterator<Item = Option<String>>) -> Self {
        ScriptedAsker {
            answers: answers.into_iter().collect(),
            prompts: Vec::new(),
        }
    }
}

impl Asker for ScriptedAsker {
    fn ask(&mut self, prompt: &str, default: Option<&str>) -> Result<Option<String>> {
        self.prompts.push(match default {
            Some(d) => format!("{prompt} [{d}]"),
            None => prompt.to_string(),
        });
        Ok(self.answers.pop_front().flatten())
    }
}

/// Display/refresh options for `mox check`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions {
    pub json: bool,
    pub oneline: bool,
    pub watch: bool,
    pub interval: u64,
    pub all: bool,
    pub no_sync: bool,
    pub top: Option<usize>,
    pub context: bool,
}

/// Options for `mox plan`.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    pub ideas: Option<PathBuf>,
    pub answers: Option<PathBuf>,
    pub refresh: bool,
    pub yes: bool,
    pub interactive: bool,
    pub deep: bool,
}

/// Options for `mox up`.
#[derive(Debug, Clone)]
pub struct UpOptions {
    pub only: Vec<String>,
    pub max_parallel: Option<usize>,
    pub no_github: bool,
    pub no_run: bool,
    pub force: bool,
    pub resume: bool,
    pub retry_failed: bool,
    pub target: String,
    pub owner: Option<String>,
    pub detach: bool,
}

impl Default for UpOptions {
    fn default() -> Self {
        UpOptions {
            only: Vec::new(),
            max_parallel: None,
            no_github: false,
            no_run: false,
            force: false,
            resume: false,
            retry_failed: false,
            target: "local".to_string(),
            owner: None,
            detach: false,
        }
    }
}

pub struct App {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub runner: Arc<dyn CommandRunner>,
    pub brain: Arc<dyn Brain>,
    pub dry_run: bool,
    pub interactive: bool,
    pub asker: Box<dyn Asker>,
    pub notifier: Arc<dyn Notifier>,
}

impl App {
    pub fn new(
        cfg: Config,
        cfg_path: PathBuf,
        runner: Arc<dyn CommandRunner>,
        brain: Arc<dyn Brain>,
        dry_run: bool,
        interactive: bool,
    ) -> Self {
        App {
            cfg,
            cfg_path,
            runner,
            brain,
            dry_run,
            interactive,
            asker: Box::new(StdinAsker),
            notifier: Arc::new(notify::NullNotifier),
        }
    }

    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    pub fn with_asker(mut self, asker: Box<dyn Asker>) -> Self {
        self.asker = asker;
        self
    }

    pub fn run(&mut self, command: Command, out: &mut dyn Write) -> Result<()> {
        match command {
            Command::Init { force, profile } => self.cmd_init(force, &profile, out),
            Command::Doctor { skip_vps } => self.cmd_doctor(skip_vps, out),
            Command::Plan {
                ideas,
                answers,
                refresh,
                yes,
                interactive,
                deep,
            } => self.cmd_plan(
                PlanOptions {
                    ideas,
                    answers,
                    refresh,
                    yes,
                    interactive,
                    deep,
                },
                out,
            ),
            Command::Up {
                only,
                max_parallel,
                no_github,
                no_run,
                force,
                resume,
                retry_failed,
                target,
                owner,
                detach,
            } => self.cmd_up(
                UpOptions {
                    only,
                    max_parallel,
                    no_github,
                    no_run,
                    force,
                    resume,
                    retry_failed,
                    target,
                    owner,
                    detach,
                },
                out,
            ),
            Command::Status {
                json,
                watch,
                interval,
                verbose,
            } => {
                if watch {
                    self.cmd_status_live(interval, out)
                } else {
                    self.cmd_status(json, verbose, out)
                }
            }
            Command::Report { out: path } => self.cmd_report(path.as_deref(), out),
            Command::Project { action } => match action {
                crate::cli::ProjectAction::List => self.cmd_status(false, false, out),
                crate::cli::ProjectAction::Archive { slug }
                | crate::cli::ProjectAction::Remove { slug } => {
                    self.cmd_project_archive(&slug, out)
                }
                crate::cli::ProjectAction::Adopt { path, slug } => {
                    self.cmd_project_adopt(&path, slug.as_deref(), out)
                }
            },
            Command::Logs { slug, lines } => self.cmd_logs(&slug, lines, out),
            Command::Reflect { slug, lines } => self.cmd_reflect(&slug, lines, out),
            Command::Clean {
                apply,
                all,
                force,
                logs_older_than_days,
            } => self.cmd_clean(apply, all, force, logs_older_than_days, out),
            Command::Events { tail } => self.cmd_events(tail, out),
            Command::Recovery { push, check, slug } => {
                self.cmd_recovery(push, check, slug.as_deref(), out)
            }
            Command::Completions { shell } => self.cmd_completions(&shell, out),
            Command::Stop { slug } => self.cmd_stop(&slug, out),
            Command::Bot { once, allow_remote } => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                if crate::bot::remote_context(&cwd) && !allow_remote && !self.cfg.allow_remote_bots
                {
                    bail!(
                        "refusing to run the Telegram bot on a remote/VPS host. \
                         mox never launches bots remotely; if a human really wants this, \
                         run `mox bot --allow-remote` (or set allow_remote_bots = true)."
                    );
                }
                if allow_remote {
                    writeln!(
                        out,
                        "warning: running the Telegram bot on a remote host because --allow-remote was passed"
                    )?;
                }
                let mut bot = crate::bot::Bot::new(&self.cfg, self.runner.clone())?;
                bot.run(once)
            }
            Command::Notify { action } => self.cmd_notify(action, out),
            Command::Stats { json, reset, yes } => self.cmd_stats(json, reset, yes, out),
            Command::Check {
                action,
                json,
                oneline,
                watch,
                interval,
                all,
                no_sync,
                top,
                context,
            } => self.cmd_check(
                action,
                CheckOptions {
                    json,
                    oneline,
                    watch,
                    interval,
                    all,
                    no_sync,
                    top,
                    context,
                },
                out,
            ),
            Command::Vps { action } => match action {
                VpsCommand::Check => self.cmd_vps_check(out),
                VpsCommand::Suggest { compute } => self.cmd_vps_suggest(&compute, out),
                VpsCommand::Allow { slug, note } => self.cmd_vps_allow(&slug, &note, out),
                VpsCommand::Revoke { slug } => self.cmd_vps_revoke(&slug, out),
                VpsCommand::Approvals => self.cmd_vps_approvals(out),
                VpsCommand::Setup => self.cmd_vps_setup(out),
                VpsCommand::Sync {
                    slug,
                    watch,
                    interval,
                } => self.cmd_vps_sync(slug.as_deref(), watch, interval, out),
                VpsCommand::Ps => self.cmd_vps_ps(out),
                VpsCommand::Cleanup { apply, yes } => self.cmd_vps_cleanup(apply, yes, out),
                VpsCommand::Plan { compute } => self.cmd_vps_plan(&compute, out),
            },
            Command::Ideas { file } => self.cmd_ideas(&file, out),
            // Handled in main before any config is loaded.
            Command::Guard { .. } => unreachable!("guard is dispatched by main"),
            Command::Milestone { .. } => unreachable!("milestone is dispatched by main"),
        }
    }

    /// Lock held by `plan`, `up` and `clean --apply` (one at a time).
    fn up_lock(&self) -> Result<LockGuard> {
        LockGuard::acquire(&self.cfg.state_dir_path().join("up.lock"))
    }

    /// Separate lock for the attention queue, so checks can be managed while
    /// agents are running.
    fn checks_lock(&self) -> Result<LockGuard> {
        LockGuard::acquire(&self.cfg.state_dir_path().join("checks.lock"))
    }

    fn load_state(&self) -> Result<State> {
        state::load(&self.cfg.state_file())
    }

    fn save_state(&self, state: &State) -> Result<()> {
        state::save(&self.cfg.state_file(), state)
    }

    /// Persist one project unless this is a dry run (dry runs never write).
    fn persist_project(&self, state: &mut State, project: ProjectState) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        state.upsert(project);
        self.save_state(state)
    }

    fn ensure_layout(&self) -> Result<()> {
        std::fs::create_dir_all(self.cfg.projects_root_path())
            .with_context(|| format!("creating {}", self.cfg.projects_root_path().display()))?;
        // Everything that can contain prompts, logs or state stays private.
        for dir in [
            self.cfg.state_dir_path(),
            self.cfg.internal_dir(),
            self.cfg.logs_dir(),
            self.cfg.run_dir(),
            self.cfg.cache_dir(),
            self.cfg.tmp_dir(),
        ] {
            crate::util::ensure_dir_private(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    fn load_plan(&self) -> Result<Plan> {
        if let Some(plan) = self.load_state()?.plan {
            return Ok(plan);
        }
        state::load_plan(&self.cfg.plan_file())?
            .context("no plan found; run `mox plan --ideas <file>` first")
    }

    fn cmd_init(&mut self, force: bool, profile: &str, out: &mut dyn Write) -> Result<()> {
        let mut cfg = self.cfg.clone();
        cfg.apply_env();
        match profile {
            "minimal" => {
                cfg.max_parallel = 1;
                cfg.min_free_disk_mb = 1024;
                cfg.mem_per_agent_mb = 512;
                cfg.disk_watchdog_secs = 0;
            }
            "paranoid" => {
                cfg.max_parallel = 1;
                cfg.reserve_cores = 2;
                cfg.min_free_disk_mb = 4096;
                cfg.mem_per_agent_mb = 1024;
                cfg.auto_approve = false;
                cfg.standalone = true;
                cfg.allow_remote_bots = false;
                cfg.extra_instructions =
                    "Paranoid profile: prefer read-only inspection; add a CHECKS.md item before risky actions."
                        .into();
            }
            _ => {}
        }
        let existed = self.cfg_path.exists();
        let wrote = cfg.write(&self.cfg_path, force)?;
        self.cfg = cfg;
        self.ensure_layout()?;
        writeln!(out, "config: {}", self.cfg_path.display())?;
        if wrote {
            writeln!(
                out,
                "{} {}",
                if existed { "rewrote" } else { "wrote" },
                self.cfg_path.display()
            )?;
            writeln!(out, "mode: 0600")?;
        } else {
            writeln!(
                out,
                "already exists (use `mox init --force` to rewrite): {}",
                self.cfg_path.display()
            )?;
        }
        writeln!(
            out,
            "projects root: {}",
            display_path(&self.cfg.projects_root_path())
        )?;
        writeln!(
            out,
            "state dir:     {}",
            display_path(&self.cfg.state_dir_path())
        )?;
        writeln!(out, "profile:       {profile}")?;
        writeln!(out, "max parallel:  {} agents", self.cfg.max_parallel)?;
        writeln!(out, "next: mox doctor, then mox plan --ideas <file>")?;
        Ok(())
    }

    fn cmd_doctor(&mut self, skip_vps: bool, out: &mut dyn Write) -> Result<()> {
        writeln!(
            out,
            "host: {}",
            crate::host::HostProfile::detect().describe()
        )?;
        let mut failures = 0usize;
        let mut checks: Vec<(bool, String)> = Vec::new();

        checks.push((
            self.cfg_path.exists(),
            format!("config {}", display_path(&self.cfg_path)),
        ));

        let opencode = CommandSpec::new(&self.cfg.opencode_bin)
            .arg("--version")
            .timeout(20);
        let opencode_run = self.runner.run(&opencode);
        let opencode_ok = opencode_run.as_ref().map(|o| o.success()).unwrap_or(false);
        let version = opencode_run
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        checks.push((
            opencode_ok,
            format!(
                "opencode binary (`{}`){}",
                self.cfg.opencode_bin,
                if version.is_empty() {
                    String::new()
                } else {
                    format!(" {version}")
                }
            ),
        ));

        let gh = CommandSpec::new(&self.cfg.gh_bin)
            .args(["auth", "status"])
            .timeout(30);
        let gh_ok = self.runner.run(&gh).map(|o| o.success()).unwrap_or(false);
        checks.push((
            gh_ok,
            format!("GitHub CLI authenticated (`{}`)", self.cfg.gh_bin),
        ));

        let name = self.runner.run(
            &CommandSpec::new(&self.cfg.git_bin)
                .args(["config", "user.name"])
                .timeout(10),
        );
        let email = self.runner.run(
            &CommandSpec::new(&self.cfg.git_bin)
                .args(["config", "user.email"])
                .timeout(10),
        );
        let identity = name
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        let email_ok = email.map(|o| o.success()).unwrap_or(false);
        checks.push((
            !identity.is_empty() && email_ok,
            format!("git identity ({identity})"),
        ));

        let df = DfProbe::new(self.runner.as_ref());
        match df.free_mb(&self.cfg.projects_root_path()) {
            Ok(free) => {
                let guard =
                    DiskGuard::new(self.cfg.min_free_disk_mb, self.cfg.project_disk_budget_mb);
                checks.push((
                    guard.ensure_free(free).is_ok(),
                    format!(
                        "disk free: {} (floor {})",
                        human_mb(free),
                        human_mb(self.cfg.min_free_disk_mb)
                    ),
                ));
            }
            Err(err) => checks.push((false, format!("disk probe failed: {err:#}"))),
        }

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        checks.push((true, format!("cores: {cores}")));
        writeln!(
            out,
            "parallelism: {} agent(s) max, {} core(s) reserved",
            self.cfg.max_parallel, self.cfg.reserve_cores
        )?;

        if !skip_vps {
            let probe = vps::probe(self.runner.as_ref(), &self.cfg.ssh_alias);
            checks.push((probe.reachable, format!("VPS: {}", vps::describe(&probe))));
        }

        let projects = crate::resources::project_dirs(&self.cfg.projects_root_path());
        if !projects.is_empty() {
            let guarded = projects
                .iter()
                .filter(|p| crate::hooks::has_hook(p) && p.join("opencode.json").exists())
                .count();
            writeln!(
                out,
                "project guardrails: {guarded}/{} project(s) have hook + opencode.json",
                projects.len()
            )?;
        }

        for (ok, label) in &checks {
            if *ok {
                writeln!(out, "  [ok]   {label}")?;
            } else {
                failures += 1;
                writeln!(out, "  [FAIL] {label}")?;
            }
        }
        if failures > 0 {
            bail!("{failures} environment check(s) failed");
        }
        writeln!(out, "all checks passed")?;
        Ok(())
    }

    fn cmd_plan(&mut self, opts: PlanOptions, out: &mut dyn Write) -> Result<()> {
        self.ensure_layout()?;
        let _lock = self.up_lock()?;
        let cached = state::load_plan(&self.cfg.plan_file())?;
        let source_name;
        let plan_hash: Option<String>;
        let host_profile: Option<crate::host::HostProfile>;

        let mut specs: Vec<ProjectSpec> = if let Some(path) = &opts.ideas {
            let ideas = ideas::load_ideas(path)?;
            let source_hash = ideas::file_hash(path);
            writeln!(
                out,
                "asking the planning model about {} project idea(s)...",
                ideas.len()
            )?;
            let host = crate::host::HostProfile::detect();
            host_profile = Some(host.clone());
            let stats_summary =
                stats::load_optional(&self.cfg.stats_file()).map(|store| stats::summarize(&store));
            let prompt =
                prompts::questions_prompt(&self.cfg, &ideas, &host, stats_summary.as_ref());
            let reply = self
                .brain
                .think(&prompt, self.cfg.brain_timeout_secs)
                .context("brainstorming failed")?;
            let assessed = prompts::parse_brain_response(&reply)?;
            source_name = path.display().to_string();
            plan_hash = source_hash;
            prompts::merge_specs(&ideas, assessed)
        } else if !opts.refresh {
            match cached {
                Some(plan) => {
                    // Warn instead of silently reusing a stale plan.
                    if let Some(hash) = &plan.source_hash {
                        if let Some(current) = ideas::file_hash(Path::new(&plan.source)) {
                            if &current != hash {
                                bail!(
                                    "the ideas file changed since this plan was built; \
                                     rerun `mox plan --ideas {} --opts.refresh`",
                                    plan.source
                                );
                            }
                        }
                    }
                    source_name = plan.source;
                    plan_hash = plan.source_hash;
                    host_profile = plan.host.clone();
                    plan.specs
                }
                None => {
                    bail!("no plan to reuse: pass `--ideas <file>` (or run `mox plan --ideas ...`)")
                }
            }
        } else {
            bail!(
                "`--opts.refresh` needs `--ideas <file>` so the model has something to think about"
            );
        };

        // Explicit answers file wins; otherwise apply the default one if it
        // exists (it is written as a template after the first plan).
        let answers_file = opts.answers.or_else(|| {
            let default = self.cfg.answers_file();
            if default.exists() {
                Some(default)
            } else {
                None
            }
        });
        if let Some(path) = &answers_file {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading answers {}", path.display()))?;
            let table: toml::value::Table = toml::from_str(&text)
                .with_context(|| format!("parsing answers {}", path.display()))?;
            for spec in &mut specs {
                spec.apply_answers(&table);
            }
            writeln!(out, "applied answers from {}", display_path(path))?;
        }

        let should_ask = opts.interactive || (!opts.yes && self.interactive);
        if should_ask {
            self.collect_answers(&mut specs, out)?;
        }

        for spec in &mut specs {
            if spec.token_estimate_model.trim().is_empty() {
                spec.token_estimate_model = self.cfg.estimate_model.clone();
            }
        }

        if opts.deep && !self.dry_run {
            writeln!(out, "refining the specification from the answers...")?;
            let prompt = prompts::refinement_prompt(&specs);
            let reply = self
                .brain
                .think(&prompt, self.cfg.brain_timeout_secs)
                .context("spec refinement failed")?;
            let refined = prompts::parse_brain_response(&reply)?;
            for project in refined {
                let Some(slug) = project.slug else {
                    continue;
                };
                let Some(spec) = specs
                    .iter_mut()
                    .find(|s| s.slug == crate::util::slugify(&slug))
                else {
                    continue;
                };
                if let Some(summary) = project.summary.filter(|s| !s.trim().is_empty()) {
                    spec.summary = summary;
                }
                if !project.acceptance_criteria.is_empty() {
                    spec.acceptance_criteria = project.acceptance_criteria;
                }
                if !project.risks.is_empty() {
                    spec.risks = project.risks;
                }
                if !project.assumptions.is_empty() {
                    spec.assumptions = project.assumptions;
                }
                if let Some(difficulty) = project
                    .difficulty
                    .as_deref()
                    .and_then(|d| d.parse::<crate::model::Complexity>().ok())
                {
                    spec.difficulty = difficulty;
                }
                if let Some(tokens) = project.estimated_tokens.filter(|tokens| *tokens > 0) {
                    spec.estimated_tokens = crate::estimate::clamp_tokens(tokens);
                }
            }
            writeln!(out, "spec refined for {} project(s)", specs.len())?;
        }

        for spec in &mut specs {
            if spec.token_estimate_model.trim().is_empty() {
                spec.token_estimate_model = self.cfg.estimate_model.clone();
            }
            if spec.estimated_tokens == 0 {
                spec.estimated_tokens =
                    crate::estimate::tokens_from_secs(crate::estimate::planned_secs(spec));
            }
        }

        let plan = Plan {
            created_at: now_rfc3339(),
            source: source_name,
            source_hash: plan_hash,
            host: host_profile,
            specs,
        };
        state::save_plan(&self.cfg.plan_file(), &plan)?;

        crate::util::write_private(
            &self.cfg.questions_file(),
            prompts::render_questions_markdown(&plan.specs),
        )?;
        if !self.cfg.answers_file().exists() {
            crate::util::write_private(
                &self.cfg.answers_file(),
                prompts::render_answers_template(&plan.specs),
            )?;
        }

        let mut state = self.load_state()?;
        for spec in &plan.specs {
            let mut project = state.project(&spec.slug).cloned().unwrap_or_else(|| {
                ProjectState::new(
                    &spec.slug,
                    &spec.title,
                    &self
                        .cfg
                        .projects_root_path()
                        .join(&spec.slug)
                        .display()
                        .to_string(),
                )
            });
            project.title = spec.title.clone();
            project.compute = spec.compute;
            project.needs_vps = spec.needs_vps;
            state.upsert(project);
        }
        state.plan = Some(plan.clone());
        self.save_state(&state)?;
        let _ = events::record(
            &self.cfg,
            "plan",
            "",
            &format!("{} project(s)", plan.specs.len()),
        );

        writeln!(out)?;
        if let Some(host) = &plan.host {
            writeln!(out, "estimates for: {}", host.describe())?;
        }
        if !self.cfg.estimate_model.trim().is_empty() {
            writeln!(
                out,
                "token estimates for: {}",
                self.cfg.estimate_model.trim()
            )?;
        }
        writeln!(out, "plan for {} project(s):", plan.specs.len())?;
        writeln!(
            out,
            "  {:<28} {:<12} {:<8} {:<8} {:<8} title",
            "slug", "difficulty", "time", "tokens", "questions"
        )?;
        for spec in &plan.specs {
            let unanswered = spec.unanswered().len();
            writeln!(
                out,
                "  {:<28} {:<12} {:<8} {:<8} {:<8} {}",
                spec.slug,
                spec.difficulty.as_str(),
                format_secs(crate::estimate::planned_secs(spec)),
                format!("~{}", format_tokens(crate::estimate::planned_tokens(spec))),
                format!(
                    "{}/{}",
                    spec.questions.len() - unanswered,
                    spec.questions.len()
                ),
                truncate_chars(&spec.title, 48)
            )?;
        }
        writeln!(
            out,
            "\nquestions: {}",
            display_path(&self.cfg.questions_file())
        )?;
        writeln!(out, "answers:   {}", display_path(&self.cfg.answers_file()))?;
        if !self.dry_run {
            let unanswered: usize = plan.specs.iter().map(|s| s.unanswered().len()).sum();
            if unanswered > 0 {
                let mut body = Vec::new();
                for spec in &plan.specs {
                    let missing = spec.unanswered().len();
                    if missing > 0 {
                        body.push(format!(
                            "{}: {missing} question(s) — {}",
                            spec.slug,
                            crate::util::truncate_chars(&spec.title, 60)
                        ));
                    }
                }
                body.push(String::new());
                body.push(format!(
                    "Answer here: {}",
                    display_path(&self.cfg.answers_file())
                ));
                self.announce(
                    Event::new(
                        EventKind::Questions,
                        format!("{unanswered} question(s) awaiting your answers"),
                        body.join("\n"),
                    ),
                    out,
                );
            }
        }
        writeln!(
            out,
            "next: mox up{}",
            if self.dry_run { " (dry run)" } else { "" }
        )?;
        Ok(())
    }

    fn collect_answers(&mut self, specs: &mut [ProjectSpec], out: &mut dyn Write) -> Result<()> {
        let pending: Vec<(usize, String)> = specs
            .iter()
            .enumerate()
            .flat_map(|(si, spec)| {
                spec.unanswered()
                    .iter()
                    .map(move |q| (si, q.id.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        if pending.is_empty() {
            writeln!(out, "no open questions; using the plan as is")?;
            return Ok(());
        }
        writeln!(
            out,
            "\n{} question(s). Press Enter to accept the default.\n",
            pending.len()
        )?;
        for (si, qid) in pending {
            let spec = &mut specs[si];
            let Some(question) = spec.questions.iter().find(|q| q.id == qid).cloned() else {
                continue;
            };
            let prompt = format!("[{}] {} {}", spec.slug, question.id, question.question);
            if let Some(answer) = self.asker.ask(&prompt, question.default.as_deref())? {
                spec.answers.insert(qid.clone(), answer);
            } else if let Some(default) = question.default.clone() {
                spec.answers.insert(qid, default);
            }
        }
        Ok(())
    }

    fn cmd_up(&mut self, opts: UpOptions, out: &mut dyn Write) -> Result<()> {
        crate::shutdown::clear();
        self.ensure_layout()?;
        let _lock = self.up_lock()?;
        let mut state = self.load_state()?;
        let plan = self.load_plan()?;

        let specs: Vec<ProjectSpec> = if opts.only.is_empty() {
            plan.specs.clone()
        } else {
            for slug in &opts.only {
                if plan.get(slug).is_none() {
                    bail!(
                        "unknown project `{slug}`; known: {}",
                        plan.specs
                            .iter()
                            .map(|s| s.slug.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            plan.specs
                .iter()
                .filter(|s| opts.only.contains(&s.slug))
                .cloned()
                .collect()
        };
        let mut specs = specs;
        if opts.retry_failed {
            specs.retain(|spec| {
                state
                    .project(&spec.slug)
                    .map(|p| matches!(p.status, ProjectStatus::Failed | ProjectStatus::Stopped))
                    .unwrap_or(false)
            });
            if specs.is_empty() {
                writeln!(out, "nothing to retry: no failed or stopped project")?;
                return Ok(());
            }
        } else if opts.resume {
            specs.retain(|spec| {
                state
                    .project(&spec.slug)
                    .map(|p| p.status != ProjectStatus::Completed)
                    .unwrap_or(true)
            });
            if specs.is_empty() {
                writeln!(
                    out,
                    "nothing to resume: every selected project is completed"
                )?;
                return Ok(());
            }
        }
        if specs.is_empty() {
            bail!("the plan has no projects");
        }

        if !opts.force {
            let mut skipped = Vec::new();
            specs.retain(|spec| {
                let blocked = state
                    .project(&spec.slug)
                    .map(|p| {
                        p.attempts >= self.cfg.max_attempts
                            && matches!(p.status, ProjectStatus::Failed | ProjectStatus::Stopped)
                    })
                    .unwrap_or(false);
                if blocked {
                    skipped.push(spec.slug.clone());
                }
                !blocked
            });
            for slug in skipped {
                writeln!(
                    out,
                    "  {slug}: max attempts ({}) reached; pass --force to retry",
                    self.cfg.max_attempts
                )?;
            }
            if specs.is_empty() {
                writeln!(out, "no runnable project left (max attempts or filters)")?;
                return Ok(());
            }
        }

        if opts.retry_failed && !opts.no_run && !self.dry_run {
            for spec in &specs {
                if let Some(project) = state.project(&spec.slug) {
                    if project.attempts > 0 {
                        let factor = 1u64 << (project.attempts.saturating_sub(1).min(6));
                        let delay = (self.cfg.retry_backoff_secs * factor).min(60);
                        if delay > 0 {
                            writeln!(
                                out,
                                "  retrying {} after {delay}s (attempt {})",
                                spec.slug,
                                project.attempts + 1
                            )?;
                            std::thread::sleep(std::time::Duration::from_secs(delay));
                        }
                    }
                }
            }
        }

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let scheduler = Scheduler::new(
            cores,
            self.cfg.reserve_cores,
            opts.max_parallel.unwrap_or(self.cfg.max_parallel),
        );

        if !self.dry_run {
            let df = DfProbe::new(self.runner.as_ref());
            let free = df.free_mb(&self.cfg.projects_root_path())?;
            let guard = DiskGuard::new(self.cfg.min_free_disk_mb, self.cfg.project_disk_budget_mb);
            if !opts.force {
                guard.ensure_free(free)?;
            } else if free < self.cfg.min_free_disk_mb {
                writeln!(
                    out,
                    "warning: only {} free (floor {}), continuing because --force",
                    human_mb(free),
                    human_mb(self.cfg.min_free_disk_mb)
                )?;
            }
        }

        writeln!(
            out,
            "preparing {} project(s): {} agent(s) in parallel, {} core(s) each on average",
            specs.len(),
            scheduler.max_parallel(),
            scheduler.usable_cores() / scheduler.max_parallel().max(1)
        )?;
        for spec in &specs {
            let approved = state
                .project(&spec.slug)
                .and_then(|p| p.vps_approved_at.as_deref());
            if spec.needs_vps && approved.is_none() {
                writeln!(
                    out,
                    "  note: `{}` needs VPS work but is not approved; run \
                     `mox vps allow {}` once before any remote step.",
                    spec.slug, spec.slug
                )?;
            } else if let Some(when) = approved {
                writeln!(
                    out,
                    "  note: `{}` is approved for VPS work since {when}",
                    spec.slug
                )?;
            }
            if matches!(spec.compute, ComputeNeed::Heavy | ComputeNeed::Gpu) {
                writeln!(
                    out,
                    "  note: `{}` is a {} workload; check `mox vps suggest --compute {}` \
                     before long runs (local disk/RAM are shared).",
                    spec.slug,
                    spec.compute.as_str(),
                    spec.compute.as_str()
                )?;
            }
        }

        let mut runnable: Vec<ProjectSpec> = Vec::new();
        for spec in &specs {
            let project_dir = self.cfg.projects_root_path().join(&spec.slug);
            let mut project = state.project(&spec.slug).cloned().unwrap_or_else(|| {
                ProjectState::new(&spec.slug, &spec.title, &project_dir.display().to_string())
            });
            project.title = spec.title.clone();
            project.compute = spec.compute;
            project.needs_vps = spec.needs_vps;
            project.path = project_dir.display().to_string();
            project.log_path = runner::log_path(&self.cfg, &spec.slug)
                .display()
                .to_string();

            if self.dry_run {
                writeln!(
                    out,
                    "\n[dry-run] {} -> {}",
                    spec.slug,
                    display_path(&project_dir)
                )?;
                writeln!(
                    out,
                    "  would scaffold README.md, AGENTS.md, TASK.md, .gitignore"
                )?;
                writeln!(
                    out,
                    "  estimate: {}",
                    crate::estimate::describe(spec, &self.cfg)
                )?;
            } else {
                match scaffold_project(&self.cfg, spec) {
                    Ok(result) => {
                        writeln!(
                            out,
                            "\n{} -> {}{}",
                            spec.slug,
                            display_path(&result.path),
                            if result.existed {
                                " (existing workspace)"
                            } else {
                                ""
                            }
                        )?;
                        writeln!(
                            out,
                            "  estimate: {}",
                            crate::estimate::describe(spec, &self.cfg)
                        )?;
                        if !result.created.is_empty() {
                            writeln!(
                                out,
                                "  created: {}",
                                result
                                    .created
                                    .iter()
                                    .filter_map(|p| p.file_name())
                                    .map(|n| n.to_string_lossy().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )?;
                        }
                    }
                    Err(err) => {
                        project.status = ProjectStatus::Failed;
                        project.last_error = Some(format!("{err:#}"));
                        self.persist_project(&mut state, project)?;
                        writeln!(out, "\n{} -> scaffold failed: {err:#}", spec.slug)?;
                        continue;
                    }
                }
            }

            if !opts.no_github {
                let already = !self.dry_run
                    && project_dir.join(".git").exists()
                    && github::has_origin(self.runner.as_ref(), &self.cfg, &project_dir);
                if already {
                    let remote = self.runner.run(
                        &CommandSpec::new(&self.cfg.git_bin)
                            .args(["remote", "get-url", "origin"])
                            .cwd(&project_dir)
                            .timeout(30),
                    );
                    project.repo_url = remote
                        .map(|o| github::ssh_remote_to_https(o.stdout.trim()))
                        .unwrap_or_default();
                    writeln!(
                        out,
                        "  repo already configured: {}",
                        if project.repo_url.is_empty() {
                            "(origin present)"
                        } else {
                            &project.repo_url
                        }
                    )?;
                } else {
                    match github::publish_private_repo(
                        self.runner.as_ref(),
                        &self.cfg,
                        &project_dir,
                        &spec.slug,
                        opts.owner.as_deref(),
                        self.dry_run,
                    ) {
                        Ok(result) => {
                            project.repo_url = result.repo_url;
                            if self.dry_run {
                                writeln!(out, "  [dry-run] publish private repo:")?;
                                for cmd in &result.commands {
                                    writeln!(out, "    {cmd}")?;
                                }
                            } else {
                                writeln!(
                                    out,
                                    "  private repo: {}",
                                    if project.repo_url.is_empty() {
                                        "(created)"
                                    } else {
                                        &project.repo_url
                                    }
                                )?;
                            }
                        }
                        Err(err) => {
                            project.status = ProjectStatus::Failed;
                            project.last_error = Some(format!("{err:#}"));
                            self.persist_project(&mut state, project)?;
                            writeln!(out, "  repo creation failed: {err:#}")?;
                            continue;
                        }
                    }
                }
            }

            project.status = ProjectStatus::Scaffolded;
            project.last_error = None;
            self.persist_project(&mut state, project)?;
            runnable.push(spec.clone());
        }

        // Anything that failed above must not be run.
        runnable.retain(|spec| {
            state
                .project(&spec.slug)
                .map(|p| p.status != ProjectStatus::Failed)
                .unwrap_or(false)
        });

        if self.cfg.recovery_enabled && !self.dry_run {
            if let Err(err) = crate::recovery::sync_all(self.runner.as_ref(), &self.cfg) {
                writeln!(out, "warning: recovery snapshot failed: {err:#}")?;
            }
        }

        if opts.no_run {
            writeln!(out, "\nno agents launched (--no-run)")?;
            self.print_summary(&state, out)?;
            return Ok(());
        }

        let target: crate::remote::Target = opts
            .target
            .parse()
            .map_err(|err: String| anyhow::anyhow!(err))?;
        if opts.detach && target != crate::remote::Target::Vps {
            bail!("`--detach` requires `--target vps` (detached runs live on the VPS)");
        }
        let mut vps_probe = None;
        let mut remote_check = None;
        if target != crate::remote::Target::Local && !runnable.is_empty() {
            vps_probe = Some(vps::probe(self.runner.as_ref(), &self.cfg.ssh_alias));
            remote_check = Some(crate::remote::check(
                self.runner.as_ref(),
                &self.cfg,
                &self.cfg.ssh_alias,
            ));
        }

        // Decide per project: local vs VPS. `--target vps` is strict; `auto`
        // silently falls back to local when approval or prerequisites are
        // missing.
        let local_capacity = vps::HostCapacity {
            cores,
            mem_mb: crate::host::mem_available_mb(),
            disk_free_mb: 4096,
        };
        let mut local_specs: Vec<ProjectSpec> = Vec::new();
        let mut remote_specs: Vec<ProjectSpec> = Vec::new();
        for spec in &runnable {
            let wants_vps = match target {
                crate::remote::Target::Local => false,
                crate::remote::Target::Vps => true,
                crate::remote::Target::Auto => matches!(
                    vps::recommend(spec.compute, local_capacity, vps_probe.as_ref()).target,
                    vps::ExecutionTarget::Vps
                ),
            };
            if wants_vps {
                let approved = state
                    .project(&spec.slug)
                    .and_then(|p| p.vps_approved_at.as_deref())
                    .is_some();
                let ready = remote_check.as_ref().map(|c| c.ready()).unwrap_or(false);
                if approved && ready {
                    remote_specs.push(spec.clone());
                    continue;
                }
                if target == crate::remote::Target::Vps {
                    let reason = if !approved {
                        "VPS deployment is not approved; run `mox vps allow <slug>` once"
                    } else {
                        "VPS is not ready for remote execution; run `mox vps setup`"
                    };
                    let mut project = state
                        .project(&spec.slug)
                        .cloned()
                        .unwrap_or_else(|| ProjectState::new(&spec.slug, &spec.title, ""));
                    project.status = ProjectStatus::Failed;
                    project.last_error = Some(reason.to_string());
                    project.target = "vps".to_string();
                    self.persist_project(&mut state, project)?;
                    writeln!(out, "  {}: {reason}", spec.slug)?;
                    continue;
                }
                writeln!(
                    out,
                    "  note: `{}` would use the VPS but is not approved/ready; running locally",
                    spec.slug
                )?;
            }
            local_specs.push(spec.clone());
        }
        if !self.dry_run {
            self.save_state(&state)?;
        }

        // Remote jobs run one at a time (each uses the whole VPS), then the
        // local pool runs in parallel.
        for spec in &remote_specs {
            if crate::shutdown::requested() {
                break;
            }
            let dir = self.cfg.projects_root_path().join(&spec.slug);
            let prompt = prompts::execution_prompt(&self.cfg, spec);
            let mut project = state
                .project(&spec.slug)
                .cloned()
                .unwrap_or_else(|| ProjectState::new(&spec.slug, &spec.title, ""));
            project.target = "vps".to_string();
            project.status = ProjectStatus::Running;
            project.attempts = project.attempts.saturating_add(1);
            project.cores = Vec::new();
            if !self.dry_run {
                state.upsert(project.clone());
                self.save_state(&state)?;
            }
            writeln!(
                out,
                "\n{} on VPS `{}` (remote log: {})",
                spec.slug,
                self.cfg.ssh_alias,
                display_path(&runner::log_path(&self.cfg, &spec.slug))
            )?;
            if self.dry_run {
                writeln!(
                    out,
                    "  [dry-run] {}",
                    crate::remote::rsync_to_spec(&self.cfg, &dir, &self.cfg.ssh_alias, &spec.slug)
                        .display()
                )?;
                writeln!(
                    out,
                    "  [dry-run] {}",
                    crate::remote::remote_run_spec(
                        &self.cfg,
                        &self.cfg.ssh_alias,
                        &spec.slug,
                        &prompt,
                        self.cfg.run_timeout_secs
                    )
                    .display()
                )?;
                continue;
            }
            if opts.detach {
                match crate::remote::start_detached(
                    self.runner.as_ref(),
                    &self.cfg,
                    spec,
                    &dir,
                    &prompt,
                    &self.cfg.ssh_alias,
                ) {
                    Ok(pid) => {
                        project.remote_pid = Some(pid);
                        project.log_path = runner::log_path(&self.cfg, &spec.slug)
                            .with_extension("remote.log")
                            .display()
                            .to_string();
                        writeln!(
                            out,
                            "  {}: detached on VPS (pid {pid}); it continues if this computer shuts down",
                            spec.slug
                        )?;
                        writeln!(
                            out,
                            "  run `mox vps sync --slug {}` later to fetch the result",
                            spec.slug
                        )?;
                    }
                    Err(err) => {
                        project.status = ProjectStatus::Failed;
                        project.last_error = Some(format!("could not start detached run: {err:#}"));
                        writeln!(out, "  {}: {err:#}", spec.slug)?;
                    }
                }
                state.upsert(project);
                self.save_state(&state)?;
                continue;
            }

            let started = std::time::Instant::now();
            let result = crate::remote::execute(
                self.runner.as_ref(),
                &self.cfg,
                spec,
                &dir,
                &prompt,
                &self.cfg.ssh_alias,
            );
            project.duration_secs = started.elapsed().as_secs();
            project.pid = None;
            match result {
                Ok(remote) => {
                    project.exit_code = remote.output.code;
                    project.log_path = remote.log_path.display().to_string();
                    if remote.output.cancelled {
                        project.status = ProjectStatus::Stopped;
                        project.last_error = Some("remote run cancelled by user".to_string());
                        writeln!(out, "  {}: stopped by user", spec.slug)?;
                    } else if !remote.output.success() {
                        project.status = ProjectStatus::Failed;
                        let reason = if remote.output.timed_out {
                            format!("remote run timed out after {}s", self.cfg.run_timeout_secs)
                        } else {
                            format!(
                                "remote run failed ({}): {}",
                                remote.output.summary(),
                                remote.output.tail(4)
                            )
                        };
                        project.last_error = Some(reason.clone());
                        writeln!(out, "  {}: {reason}", spec.slug)?;
                    } else if let Some(note) = remote.merge_note {
                        project.status = ProjectStatus::Failed;
                        project.last_error = Some(note.clone());
                        writeln!(out, "  {}: {note}", spec.slug)?;
                    } else {
                        let verification =
                            crate::verify::verify(self.runner.as_ref(), &self.cfg, &dir);
                        if let Some(reason) = verification.reason() {
                            project.status = ProjectStatus::Failed;
                            project.last_error = Some(reason.clone());
                            writeln!(out, "  {}: failed ({reason})", spec.slug)?;
                        } else {
                            project.status = ProjectStatus::Completed;
                            project.last_error = None;
                            writeln!(
                                out,
                                "  {}: done in {}s (fetched back: {})",
                                spec.slug, project.duration_secs, remote.fetched
                            )?;
                        }
                    }
                }
                Err(err) => {
                    project.status = ProjectStatus::Failed;
                    project.last_error = Some(format!("remote execution failed: {err:#}"));
                    writeln!(out, "  {}: remote execution failed: {err:#}", spec.slug)?;
                }
            }
            state.upsert(project);
            self.save_state(&state)?;
        }

        // Local rolling pool.
        let needs: Vec<ComputeNeed> = local_specs.iter().map(|s| s.compute).collect();
        let mem_available = crate::host::mem_available_mb();
        let effective =
            scheduler.effective_parallel(&needs, mem_available, self.cfg.mem_per_agent_mb);
        let pool_scheduler = Scheduler::new(
            scheduler.total_cores(),
            scheduler.reserve_cores(),
            effective,
        );
        let slots = pool_scheduler.slots();
        if !local_specs.is_empty() {
            writeln!(
                out,
                "\nlocal pool: {} agent(s) at once ({} core(s) each, {} MiB RAM/agent; {} MiB available)",
                effective,
                pool_scheduler.usable_cores() / effective.max(1),
                self.cfg.mem_per_agent_mb,
                mem_available
            )?;
        }

        let mut runs = Vec::new();
        for (index, spec) in local_specs.iter().enumerate() {
            let cores_for_agent = pool_scheduler.cores_for(index % effective.max(1));
            let dir = self.cfg.projects_root_path().join(&spec.slug);
            let prompt = prompts::execution_prompt(&self.cfg, spec);
            let run = ProjectRun::new(spec, dir, prompt, cores_for_agent.clone());

            let mut project = state
                .project(&spec.slug)
                .cloned()
                .unwrap_or_else(|| ProjectState::new(&spec.slug, &spec.title, ""));
            project.cores = cores_for_agent.clone();
            project.target = "local".to_string();
            project.status = ProjectStatus::Running;
            project.attempts = project.attempts.saturating_add(1);
            if !self.dry_run {
                state.upsert(project);
            }
            writeln!(
                out,
                "  {} on core(s) {}",
                spec.slug,
                cores_for_agent
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
            runs.push(run);
        }
        if !self.dry_run && !local_specs.is_empty() {
            self.save_state(&state)?;
        }

        if self.dry_run {
            for run in &runs {
                writeln!(
                    out,
                    "  [dry-run] {}",
                    runner::build_run_command(&self.cfg, run).display()
                )?;
            }
        } else if !local_specs.is_empty() && !crate::shutdown::requested() {
            let watchdog_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let watchdog_msg = Arc::new(std::sync::Mutex::new(None::<String>));
            let watchdog = if self.cfg.disk_watchdog_secs > 0 {
                let cfg = self.cfg.clone();
                let runner = self.runner.clone();
                let stop = watchdog_stop.clone();
                let msg = watchdog_msg.clone();
                Some(std::thread::spawn(move || {
                    let probe = DfProbe::new(runner.as_ref());
                    loop {
                        for _ in 0..cfg.disk_watchdog_secs {
                            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(1));
                        }
                        if stop.load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        if let Ok(free) = probe.free_mb(&cfg.projects_root_path()) {
                            if free < cfg.min_free_disk_mb {
                                *msg.lock().unwrap() = Some(format!(
                                    "disk watchdog stopped local agents: {} free < {} floor",
                                    human_mb(free),
                                    human_mb(cfg.min_free_disk_mb)
                                ));
                                crate::shutdown::request_disk();
                                return;
                            }
                        }
                    }
                }))
            } else {
                None
            };
            let recovery_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let recovery_msg = Arc::new(std::sync::Mutex::new(None::<String>));
            let recovery_thread = if self.cfg.recovery_enabled
                && self.cfg.recovery_interval_secs > 0
            {
                let cfg = self.cfg.clone();
                let runner = self.runner.clone();
                let stop = recovery_stop.clone();
                let msg = recovery_msg.clone();
                Some(std::thread::spawn(move || loop {
                    for _ in 0..cfg.recovery_interval_secs {
                        if stop.load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    if stop.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    if let Err(err) = crate::recovery::sync_all(runner.as_ref(), &cfg) {
                        *msg.lock().unwrap() = Some(format!("recovery snapshot failed: {err:#}"));
                    }
                }))
            } else {
                None
            };
            let outcomes =
                runner::execute_pool(self.runner.as_ref(), &self.cfg, runs, effective, &slots);
            for outcome in outcomes {
                let mut project = state
                    .project(&outcome.slug)
                    .cloned()
                    .unwrap_or_else(|| ProjectState::new(&outcome.slug, "", ""));
                project.exit_code = outcome.output.code;
                project.pid = None;
                project.duration_secs = outcome.duration_secs;
                project.cores = outcome.cores.clone();
                let verification = if outcome.success() {
                    let dir = self.cfg.projects_root_path().join(&outcome.slug);
                    Some(crate::verify::verify(self.runner.as_ref(), &self.cfg, &dir))
                } else {
                    None
                };
                if outcome.output.cancelled {
                    project.status = ProjectStatus::Stopped;
                    project.last_error = Some(match crate::shutdown::reason() {
                        crate::shutdown::StopReason::Disk => {
                            "stopped by disk watchdog: free space below floor".to_string()
                        }
                        _ => "cancelled by user (SIGINT/SIGTERM)".to_string(),
                    });
                    writeln!(
                        out,
                        "  {}: stopped ({})",
                        outcome.slug,
                        project.last_error.as_deref().unwrap_or("")
                    )?;
                } else {
                    match verification {
                        Some(verification) if verification.reason().is_none() => {
                            project.status = ProjectStatus::Completed;
                            project.last_error = None;
                            writeln!(
                                out,
                                "  {}: done in {}s (log: {})",
                                outcome.slug,
                                outcome.duration_secs,
                                display_path(&outcome.log_path)
                            )?;
                            if let Some(note) = &verification.unverified {
                                writeln!(out, "    note: {note}")?;
                            }
                            if self.cfg.reflect_after_run {
                                self.write_auto_reflection(&outcome.slug, out)?;
                            }
                        }
                        Some(verification) => {
                            let reason = verification.reason().unwrap_or_default();
                            project.status = ProjectStatus::Failed;
                            project.last_error = Some(reason.clone());
                            writeln!(out, "  {}: failed ({reason})", outcome.slug)?;
                        }
                        None => {
                            project.status = ProjectStatus::Failed;
                            let reason = if outcome.output.timed_out {
                                format!("timed out after {}s", self.cfg.run_timeout_secs)
                            } else {
                                format!("{}: {}", outcome.output.summary(), outcome.output.tail(3))
                            };
                            project.last_error = Some(reason.clone());
                            writeln!(out, "  {}: failed ({reason})", outcome.slug)?;
                        }
                    }
                }
                let status = project.status.as_str().to_string();
                let slug = project.slug.clone();
                state.upsert(project);
                let _ = events::record(&self.cfg, "run", &slug, &status);
            }
            recovery_stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(thread) = recovery_thread {
                let _ = thread.join();
                if let Some(message) = recovery_msg.lock().unwrap().take() {
                    writeln!(out, "\nwarning: {message}")?;
                }
            }
            watchdog_stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(watchdog) = watchdog {
                let _ = watchdog.join();
                if let Some(message) = watchdog_msg.lock().unwrap().take() {
                    writeln!(out, "\n{message}")?;
                }
            }
            self.save_state(&state)?;
        }

        if self.cfg.recovery_enabled && !self.dry_run {
            let _ = crate::recovery::sync_all(self.runner.as_ref(), &self.cfg);
        }

        if !self.dry_run {
            let mut lines = Vec::new();
            let mut completed = 0usize;
            let mut failed = 0usize;
            for spec in &specs {
                let Some(project) = state.project(&spec.slug) else {
                    continue;
                };
                if project.status == ProjectStatus::Completed {
                    completed += 1;
                }
                if project.status == ProjectStatus::Failed {
                    failed += 1;
                }
                lines.push(format!(
                    "{}: {} ({}s){}",
                    spec.slug,
                    project.status.as_str(),
                    project.duration_secs,
                    project
                        .last_error
                        .as_deref()
                        .map(|e| format!(" — {e}"))
                        .unwrap_or_default()
                ));
            }
            if let Ok(store) = checks::load(&self.cfg.checks_file()) {
                let open = store.open().len();
                if open > 0 {
                    lines.push(format!("{} open human check(s) — run `mox check`", open));
                }
            }
            let kind = if failed > 0 {
                EventKind::Failed
            } else {
                EventKind::Finished
            };
            self.announce(
                Event::new(
                    kind,
                    format!("{completed} completed, {failed} failed"),
                    lines.join("\n"),
                ),
                out,
            );
        }
        if !self.dry_run {
            let now = Utc::now();
            let mut lines = Vec::new();
            for spec in &specs {
                let dir = self.cfg.projects_root_path().join(&spec.slug);
                if let Some(board) = milestones::load_optional(&milestones::board_path(&dir)) {
                    let progress = milestones::progress(&board, now);
                    lines.push(format!(
                        "  {:<20} {:<10} ETA {}",
                        progress.slug,
                        progress.short(),
                        progress.eta_label()
                    ));
                }
            }
            if !lines.is_empty() {
                writeln!(out, "\nmilestones:")?;
                for line in lines {
                    writeln!(out, "{line}")?;
                }
            }
        }
        if self.dry_run {
            writeln!(
                out,
                "\ndry run: no files created, no commands executed, no state written"
            )?;
        } else {
            // Agents may have recorded human checks while they worked.
            let now = Utc::now();
            let mut store = checks::load(&self.cfg.checks_file())?;
            match checks::sync_projects(&mut store, &self.cfg.projects_root_path(), now) {
                Ok(outcome) if outcome.total() > 0 => {
                    checks::save(&self.cfg.checks_file(), &store)?;
                    writeln!(
                        out,
                        "\nattention: {} new human check(s), {} refreshed — run `mox check`",
                        outcome.created, outcome.updated
                    )?;
                }
                Ok(_) => {}
                Err(err) => writeln!(out, "warning: could not import CHECKS.md: {err:#}")?,
            }
        }
        self.print_summary(&state, out)?;
        Ok(())
    }

    fn print_summary(&self, state: &State, out: &mut dyn Write) -> Result<()> {
        writeln!(out, "\nsummary:")?;
        for project in state.ordered() {
            if project.status == ProjectStatus::Planned {
                continue;
            }
            writeln!(
                out,
                "  {:<28} {:<11} {}",
                project.slug,
                project.status.as_str(),
                project.last_error.as_deref().unwrap_or("")
            )?;
        }
        writeln!(
            out,
            "\nlogs: {}, status: mox status",
            display_path(&self.cfg.logs_dir())
        )?;
        Ok(())
    }

    fn cmd_status(&mut self, json: bool, verbose: bool, out: &mut dyn Write) -> Result<()> {
        let state = self.load_state()?;
        if json {
            let mut value = serde_json::to_value(&state)?;
            let mut progress_map = serde_json::Map::new();
            let now = Utc::now();
            for project in state.ordered() {
                let dir = PathBuf::from(&project.path);
                if let Some(board) = milestones::load_optional(&milestones::board_path(&dir)) {
                    progress_map.insert(
                        project.slug.clone(),
                        serde_json::to_value(milestones::progress(&board, now))?,
                    );
                }
            }
            value["milestone_progress"] = serde_json::Value::Object(progress_map);
            writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
            return Ok(());
        }
        if state.projects.is_empty() {
            writeln!(
                out,
                "no projects yet; run `mox plan --ideas <file>` then `mox up`"
            )?;
            return Ok(());
        }
        let guard = DiskGuard::new(self.cfg.min_free_disk_mb, self.cfg.project_disk_budget_mb);
        if let Ok(store) = checks::load(&self.cfg.checks_file()) {
            writeln!(
                out,
                "{} (details: mox check)\n",
                checks::oneline(&store, Utc::now())
            )?;
        }
        writeln!(
            out,
            "{:<28} {:<11} {:<8} {:<7} {:>9} {:>7} {:>12} {:>16}  repo",
            "slug", "status", "compute", "cores", "size", "time", "progress", "eta"
        )?;
        for project in state.ordered() {
            let dir = PathBuf::from(&project.path);
            let bytes = if dir.is_dir() {
                dir_size_bytes(&dir)
            } else {
                0
            };
            let mb = bytes / (1024 * 1024);
            let size = if guard.over_budget(mb) {
                format!("{}!", human_mb(mb))
            } else {
                human_mb(mb)
            };
            let cores = if project.cores.is_empty() {
                "-".to_string()
            } else {
                project
                    .cores
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let pid = runner::read_pid(&self.cfg, &project.slug);
            let detached = project.target == "vps" && project.remote_pid.is_some();
            let stale = project.status == ProjectStatus::Running && pid.is_none() && !detached;
            let running = pid
                .map(|pid| format!(" (pid {pid})"))
                .or_else(|| project.remote_pid.map(|pid| format!(" (remote {pid})")))
                .unwrap_or_default();
            let status_label = if stale {
                "stale".to_string()
            } else {
                format!("{}{}", project.status.as_str(), running)
            };
            let milestone_progress = milestones::load_optional(&milestones::board_path(&dir))
                .map(|board| milestones::progress(&board, Utc::now()));
            let (progress_label, eta_label) = match &milestone_progress {
                Some(progress) => (progress.short(), progress.compact_eta()),
                None => ("-".to_string(), "-".to_string()),
            };
            writeln!(
                out,
                "{:<28} {:<11} {:<8} {:<7} {:>9} {:>7} {:>12} {:>16}  {}{}",
                project.slug,
                truncate_chars(&status_label, 11),
                project.compute.as_str(),
                cores,
                size,
                if project.duration_secs > 0 {
                    format!("{}s", project.duration_secs)
                } else {
                    "-".to_string()
                },
                truncate_chars(&progress_label, 12),
                truncate_chars(&eta_label, 16),
                truncate_chars(&project.repo_url, 44),
                if project.last_error.is_some() {
                    " !"
                } else if project.vps_approved_at.is_some() {
                    " [vps-ok]"
                } else {
                    ""
                }
            )?;
        }
        if state.projects.values().any(|p| {
            p.status == ProjectStatus::Running && runner::read_pid(&self.cfg, &p.slug).is_none()
        }) {
            writeln!(
                out,
                "\nstale running entries: the agent is gone. Use `mox up --resume` or `mox stop <slug>`."
            )?;
        }
        if verbose {
            for project in state.ordered() {
                let path = PathBuf::from(&project.path).join("STATUS.md");
                writeln!(out, "\n{} — {}", project.slug, display_path(&path))?;
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        for line in text.lines().filter(|l| !l.trim().is_empty()).take(6) {
                            writeln!(out, "  {line}")?;
                        }
                    }
                    Err(_) => writeln!(out, "  (no STATUS.md)")?,
                }
            }
        }
        Ok(())
    }

    fn cmd_project_adopt(
        &mut self,
        path: &Path,
        slug_override: Option<&str>,
        out: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_layout()?;
        let _lock = self.up_lock()?;
        let root = self.cfg.projects_root_path();
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        if !path.is_dir() {
            bail!("not a directory: {}", path.display());
        }
        if !crate::util::is_inside(&root, &path) {
            bail!(
                "adopt only supports directories inside {} (move it there first)",
                display_path(&root)
            );
        }
        let slug = match slug_override {
            Some(slug) => crate::util::slugify(slug),
            None => path
                .file_name()
                .map(|n| crate::util::slugify(&n.to_string_lossy()))
                .unwrap_or_else(|| "project".to_string()),
        };
        if slug == "project" {
            bail!("could not derive a slug; pass --slug");
        }
        if root.join(&slug) != path {
            bail!("--slug must match the directory name for adopt");
        }
        let mut state = self.load_state()?;
        if state.project(&slug).is_some() {
            bail!("project `{slug}` already exists in state");
        }
        let mut plan = self.load_plan().unwrap_or_default();
        let idea = crate::model::Idea {
            title: Some(slug.clone()),
            details: format!("Adopted existing workspace at {}", path.display()),
            raw: String::new(),
        };
        let mut spec = crate::model::ProjectSpec::from_idea(&idea, slug.clone());
        spec.milestones = crate::milestones::fallback_for(&spec);
        spec.difficulty = crate::estimate::difficulty_from_milestones(&spec.milestones);
        spec.estimated_tokens =
            crate::estimate::tokens_from_secs(crate::estimate::planned_secs(&spec));
        spec.token_estimate_model = self.cfg.estimate_model.clone();
        if plan.get(&slug).is_none() {
            plan.specs.push(spec.clone());
            plan.created_at = now_rfc3339();
            plan.source = plan.source.clone();
            state::save_plan(&self.cfg.plan_file(), &plan)?;
        }
        let scaffolding = scaffold_project(&self.cfg, &spec)?;
        let mut project =
            ProjectState::new(&slug, &spec.title, &scaffolding.path.display().to_string());
        project.status = ProjectStatus::Scaffolded;
        state.upsert(project);
        state.plan = Some(plan);
        self.save_state(&state)?;
        writeln!(
            out,
            "adopted {slug} -> {} ({} new file(s))",
            display_path(&scaffolding.path),
            scaffolding.created.len()
        )?;
        writeln!(
            out,
            "  estimate: {}",
            crate::estimate::describe(&spec, &self.cfg)
        )?;
        Ok(())
    }

    fn cmd_project_archive(&mut self, slug: &str, out: &mut dyn Write) -> Result<()> {
        self.ensure_layout()?;
        let mut state = self.load_state()?;
        let Some(mut project) = state.project(slug).cloned() else {
            bail!("unknown project `{slug}`");
        };
        let source = self.cfg.projects_root_path().join(slug);
        if !source.is_dir() {
            bail!("project directory is missing: {}", source.display());
        }
        if project.status == ProjectStatus::Running && runner::read_pid(&self.cfg, slug).is_some() {
            bail!("`{slug}` is still running; stop it before archiving");
        }
        let archive_root = self.cfg.internal_dir().join("archive");
        crate::util::ensure_dir_private(&archive_root)?;
        let stamp = crate::util::timestamp_millis();
        let mut destination = archive_root.join(format!("{slug}-{stamp}"));
        let mut counter = 2;
        while destination.exists() {
            destination = archive_root.join(format!("{slug}-{stamp}-{counter}"));
            counter += 1;
        }
        if !crate::util::is_inside(&self.cfg.projects_root_path(), &source) {
            bail!("refusing to archive outside the projects root");
        }
        std::fs::rename(&source, &destination)
            .with_context(|| format!("moving {} to {}", source.display(), destination.display()))?;
        project.status = ProjectStatus::Archived;
        project.path = destination.display().to_string();
        project.pid = None;
        project.updated_at = now_rfc3339();
        state.upsert(project);
        self.save_state(&state)?;
        writeln!(
            out,
            "archived {slug} -> {} (the GitHub repository is untouched)",
            display_path(&destination)
        )?;
        Ok(())
    }

    fn cmd_report(&mut self, path: Option<&Path>, out: &mut dyn Write) -> Result<()> {
        let state = self.load_state()?;
        let checks = checks::load(&self.cfg.checks_file()).unwrap_or_default();
        let stats_store = stats::load(&self.cfg.stats_file()).unwrap_or_default();
        let now = Utc::now();
        let mut report = String::new();
        report.push_str("# mox report\n\n");
        report.push_str(&format!(
            "Generated {} · host: {}\n\n",
            now_rfc3339(),
            crate::host::HostProfile::detect().describe()
        ));
        report.push_str(&format!(
            "{} ({} projects)\n\n",
            checks::oneline(&checks, now),
            state.projects.len()
        ));
        report
            .push_str("| project | status | target | milestone | eta | attempts | size | repo |\n");
        report.push_str("|---|---|---|---|---|---|---|---|\n");
        for project in state.ordered() {
            let dir = PathBuf::from(&project.path);
            let board = milestones::load_optional(&milestones::board_path(&dir));
            let (progress, eta) = match &board {
                Some(board) => {
                    let p = milestones::progress(board, now);
                    (p.short(), p.compact_eta())
                }
                None => ("-".to_string(), "-".to_string()),
            };
            let size = dir_size_bytes(Path::new(&project.path));
            report.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                project.slug,
                project.status.as_str(),
                if project.target.is_empty() {
                    "local"
                } else {
                    &project.target
                },
                progress,
                eta,
                project.attempts,
                human_mb(size / (1024 * 1024)),
                project.repo_url
            ));
            if let Some(error) = &project.last_error {
                report.push_str(&format!("\n**{} error:** {}\n", project.slug, error));
            }
        }
        report.push_str("\n## Status excerpts\n");
        for project in state.ordered() {
            let status = std::fs::read_to_string(PathBuf::from(&project.path).join("STATUS.md"));
            report.push_str(&format!("\n### {}\n\n", project.slug));
            match status {
                Ok(text) => {
                    for line in text.lines().take(12) {
                        report.push_str(line);
                        report.push('\n');
                    }
                }
                Err(_) => report.push_str("(no STATUS.md)\n"),
            }
        }
        report.push_str("\n## Timing statistics\n\n```\n");
        report.push_str(&stats::render(&stats::summarize(&stats_store)));
        report.push_str("```\n");
        match path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, report)?;
                writeln!(out, "wrote {}", display_path(path))?;
            }
            None => write!(out, "{report}")?,
        }
        Ok(())
    }

    fn cmd_status_live(&mut self, interval: u64, out: &mut dyn Write) -> Result<()> {
        let interval = interval.max(1);
        loop {
            write!(out, "\x1b[2J\x1b[H")?;
            self.cmd_status(false, false, out)?;
            writeln!(
                out,
                "\nrefreshing every {interval}s — Ctrl-C to quit · ETA is weighted by milestone complexity"
            )?;
            out.flush()?;
            std::thread::sleep(std::time::Duration::from_secs(interval));
        }
    }

    fn cmd_logs(&mut self, slug: &str, lines: usize, out: &mut dyn Write) -> Result<()> {
        let path = runner::log_path(&self.cfg, slug);
        let tail = tail_lines(&path, lines)?;
        if tail.is_empty() {
            writeln!(out, "no log for `{slug}` at {}", display_path(&path))?;
            return Ok(());
        }
        for line in tail {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }

    fn write_auto_reflection(&mut self, slug: &str, out: &mut dyn Write) -> Result<()> {
        let plan = self.load_plan()?;
        let spec = plan
            .get(slug)
            .with_context(|| format!("unknown project `{slug}`"))?
            .clone();
        let dir = self.cfg.projects_root_path().join(slug);
        let status = std::fs::read_to_string(dir.join("STATUS.md")).unwrap_or_default();
        let log = tail_lines(&runner::log_path(&self.cfg, slug), 60)?.join("\n");
        let reply = self.brain.think(
            &prompts::reflection_prompt(&spec, &status, &log),
            self.cfg.brain_timeout_secs,
        )?;
        let path = self
            .cfg
            .internal_dir()
            .join("reflections")
            .join(format!("{slug}.md"));
        crate::util::ensure_dir_private(path.parent().unwrap())?;
        crate::util::write_private(
            &path,
            format!("# Reflection\n\n_Generated {}_\n\n{reply}\n", now_rfc3339()),
        )?;
        writeln!(out, "  reflection: {}", display_path(&path))?;
        Ok(())
    }

    fn cmd_reflect(&mut self, slug: &str, lines: usize, out: &mut dyn Write) -> Result<()> {
        let plan = self.load_plan()?;
        let spec = plan
            .get(slug)
            .with_context(|| format!("unknown project `{slug}`"))?
            .clone();
        let dir = self.cfg.projects_root_path().join(slug);
        let status_md = std::fs::read_to_string(dir.join("STATUS.md")).unwrap_or_default();
        let log_tail = tail_lines(&runner::log_path(&self.cfg, slug), lines)?.join("\n");
        let prompt = prompts::reflection_prompt(&spec, &status_md, &log_tail);
        writeln!(out, "reflecting on `{slug}`...")?;
        let reply = self
            .brain
            .think(&prompt, self.cfg.brain_timeout_secs)
            .context("reflection failed")?;
        let path = dir.join("REFLECTION.md");
        if self.dry_run {
            writeln!(out, "[dry-run] would write {}", display_path(&path))?;
            writeln!(out, "{reply}")?;
            return Ok(());
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            &path,
            format!("# Reflection\n\n_Generated {}_\n\n{reply}\n", now_rfc3339()),
        )?;
        writeln!(out, "wrote {}", display_path(&path))?;
        writeln!(out, "{reply}")?;
        Ok(())
    }

    fn cmd_clean(
        &mut self,
        apply: bool,
        all: bool,
        force: bool,
        logs_older_than_days: Option<u64>,
        out: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_layout()?;
        let _lock = if apply && !self.dry_run {
            Some(self.up_lock()?)
        } else {
            None
        };
        let report = clean::scan(&self.cfg);
        if report.project_entries.is_empty()
            && report.internal_entries.is_empty()
            && logs_older_than_days.is_none()
        {
            writeln!(out, "nothing to clean")?;
            return Ok(());
        }
        let old_logs = logs_older_than_days
            .map(|days| self.old_logs(days, out))
            .transpose()?
            .unwrap_or_default();
        writeln!(out, "{:<6} {:>10}  path", "kind", "size")?;
        for entry in &report.project_entries {
            let label = match entry.kind {
                PruneKind::Safe => "cache",
                PruneKind::Build => "build",
            };
            writeln!(
                out,
                "{:<6} {:>10}  {}",
                label,
                human_mb(entry.bytes / (1024 * 1024)),
                display_path(&entry.path)
            )?;
        }
        for entry in &report.internal_entries {
            writeln!(
                out,
                "{:<6} {:>10}  {}",
                "mox",
                human_mb(entry.bytes / (1024 * 1024)),
                display_path(&entry.path)
            )?;
        }
        writeln!(
            out,
            "\ntotal: {} (removable now: {})",
            human_mb(report.total_bytes() / (1024 * 1024)),
            human_mb(
                if all {
                    report.total_bytes()
                } else {
                    report.safe_bytes()
                } / (1024 * 1024)
            )
        )?;
        if !apply {
            writeln!(
                out,
                "dry run: nothing deleted. Re-run with `mox clean --apply`."
            )?;
            if !all {
                writeln!(out, "build output (target/, dist/, build/) needs `--all`.")?;
            }
            return Ok(());
        }
        let running = clean::running_pids(&self.cfg);
        if !running.is_empty() && !force {
            bail!(
                "agents may be running ({}); stop them or pass --force",
                running
                    .iter()
                    .map(|(s, p)| format!("{s} (pid {p})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if self.dry_run {
            writeln!(out, "[dry-run] would delete the entries above")?;
            return Ok(());
        }
        let removed = clean::apply(&self.cfg, &report, all)?;
        writeln!(out, "removed {} path(s)", removed.len())?;
        let mut log_count = 0usize;
        for path in old_logs {
            if std::fs::remove_file(&path).is_ok() {
                log_count += 1;
            }
        }
        if logs_older_than_days.is_some() {
            writeln!(out, "removed {log_count} old log/prompt file(s)")?;
        }
        Ok(())
    }

    fn old_logs(&self, days: u64, out: &mut dyn Write) -> Result<Vec<PathBuf>> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(days.saturating_mul(86_400)));
        let Some(cutoff) = cutoff else {
            return Ok(Vec::new());
        };
        let mut old = Vec::new();
        let entries = match std::fs::read_dir(self.cfg.logs_dir()) {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::now());
            if modified >= cutoff {
                continue;
            }
            writeln!(out, "old log  {}", display_path(&path))?;
            old.push(path);
        }
        Ok(old)
    }

    fn cmd_recovery(
        &mut self,
        push: bool,
        check: bool,
        slug: Option<&str>,
        out: &mut dyn Write,
    ) -> Result<()> {
        let state = self.load_state()?;
        let report = crate::recovery::render(&self.cfg, &state, Utc::now());
        if let Some(slug) = slug {
            if let Some(text) = report.projects.get(slug) {
                write!(out, "{text}")?;
            } else {
                bail!("unknown project `{slug}`");
            }
        } else {
            write!(out, "{}", report.global)?;
        }
        if push {
            let paths = crate::recovery::write_local(&self.cfg, &report)?;
            writeln!(
                out,
                "\nlocal snapshots: {} file(s) under {}",
                paths.len(),
                display_path(&self.cfg.state_dir_path().join("recovery"))
            )?;
            match crate::recovery::push(self.runner.as_ref(), &self.cfg, &report) {
                Ok(count) => writeln!(out, "pushed {count} recovery file(s) to the VPS")?,
                Err(err) => writeln!(out, "warning: recovery push failed: {err:#}")?,
            }
        }
        if check {
            match crate::recovery::remote_ages(self.runner.as_ref(), &self.cfg, &state) {
                Ok(ages) => {
                    writeln!(out, "\nremote recovery files:")?;
                    for (name, age) in ages {
                        match age {
                            Some(age) => writeln!(
                                out,
                                "  {name}: updated {} ago{}",
                                crate::util::format_secs(age),
                                if age > self.cfg.recovery_interval_secs.saturating_mul(3) {
                                    " (STALE)"
                                } else {
                                    ""
                                }
                            )?,
                            None => writeln!(out, "  {name}: MISSING")?,
                        }
                    }
                }
                Err(err) => writeln!(out, "warning: recovery check failed: {err:#}")?,
            }
        }
        Ok(())
    }

    fn cmd_completions(&mut self, shell: &str, out: &mut dyn Write) -> Result<()> {
        let commands = "init doctor plan up status logs reflect clean stop check milestone stats notify vps ideas bot events report project completions";
        match shell {
            "bash" => write!(
                out,
                "_mox() {{\n  local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n  if [ \"$COMP_CWORD\" -eq 1 ]; then\n    COMPREPLY=( $(compgen -W \"{commands}\" -- \"$cur\") )\n  fi\n}}\ncomplete -F _mox mox\n"
            )?,
            "zsh" => write!(
                out,
                "#compdef mox\n_mox() {{\n  _arguments '1:command:({commands})'\n}}\ncompdef _mox mox\n"
            )?,
            "fish" => {
                for command in commands.split_whitespace() {
                    writeln!(out, "complete -c mox -f -a {command}")?;
                }
            }
            _ => bail!("unsupported shell `{shell}`"),
        }
        Ok(())
    }

    fn cmd_events(&mut self, tail: usize, out: &mut dyn Write) -> Result<()> {
        for line in events::tail(&self.cfg, tail) {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }

    fn cmd_stop(&mut self, slug: &str, out: &mut dyn Write) -> Result<()> {
        let mut state = self.load_state()?;
        if let Some(project) = state.project(slug).cloned() {
            if project.target == "vps" && project.remote_pid.is_some() {
                if self.dry_run {
                    writeln!(out, "[dry-run] would stop remote run `{slug}`")?;
                    return Ok(());
                }
                let output = self.runner.run(&crate::remote::detached_stop_spec(
                    &self.cfg,
                    &self.cfg.ssh_alias,
                    slug,
                ))?;
                if !output.success() {
                    bail!("could not stop remote run: {}", output.tail(4));
                }
                state.update_status(slug, ProjectStatus::Stopped);
                if let Some(p) = state.project_mut(slug) {
                    p.remote_pid = None;
                }
                self.save_state(&state)?;
                writeln!(out, "stopped remote run `{slug}`")?;
                return Ok(());
            }
        }
        let Some(pid) = runner::read_pid(&self.cfg, slug) else {
            writeln!(out, "`{slug}` is not running (no pid file)")?;
            return Ok(());
        };
        if !process_is_agent(pid) {
            bail!(
                "refusing to kill pid {pid}: its command line does not look like an opencode agent"
            );
        }
        if self.dry_run {
            writeln!(out, "[dry-run] would send SIGTERM to pid {pid}")?;
            return Ok(());
        }
        let output = self.runner.run(
            &CommandSpec::new("kill")
                .args(["-TERM", &pid.to_string()])
                .timeout(10),
        )?;
        if !output.success() {
            bail!("could not stop pid {pid}: {}", output.tail(4));
        }
        runner::clear_pid_file(&self.cfg, slug);
        state.update_status(slug, ProjectStatus::Stopped);
        self.save_state(&state)?;
        writeln!(out, "sent SIGTERM to pid {pid} for `{slug}`")?;
        Ok(())
    }

    fn announce(&self, event: Event, out: &mut dyn Write) {
        if let Err(err) = self.notifier.send(&event) {
            let _ = writeln!(out, "warning: notification failed: {err:#}");
        }
    }

    fn cmd_stats(&mut self, json: bool, reset: bool, yes: bool, out: &mut dyn Write) -> Result<()> {
        if reset {
            if !yes {
                bail!("refusing to delete statistics without --yes");
            }
            match std::fs::remove_file(self.cfg.stats_file()) {
                Ok(()) => writeln!(out, "deleted {}", display_path(&self.cfg.stats_file()))?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    writeln!(out, "no statistics file to delete")?
                }
                Err(err) => return Err(err.into()),
            }
            return Ok(());
        }
        let store = stats::load(&self.cfg.stats_file())?;
        let summary = stats::summarize(&store);
        if json {
            writeln!(out, "{}", serde_json::to_string_pretty(&summary)?)?;
        } else {
            write!(out, "{}", stats::render(&summary))?;
        }
        Ok(())
    }

    fn cmd_notify(&mut self, action: NotifyAction, out: &mut dyn Write) -> Result<()> {
        match action {
            NotifyAction::Status => {
                let names = notify::channel_names(self.notifier.as_ref());
                writeln!(out, "notification channels: {names}")?;
                if names == "none" {
                    writeln!(
                        out,
                        "\nEnable one in {}:\n\
                         notify_telegram = true\n\
                         telegram_chat_id = \"123456789\"\n\
                         # export MOX_TELEGRAM_TOKEN=...   (token from @BotFather)\n\
                         # or notify_desktop = true (uses notify-send)\n\
                         # or notify_webhook = true + webhook_url = \"https://...\"",
                        display_path(&self.cfg_path)
                    )?;
                }
            }
            NotifyAction::Test => {
                let names = notify::channel_names(self.notifier.as_ref());
                if names == "none" {
                    bail!("no notification channel configured; run `mox notify status` for setup");
                }
                self.notifier
                    .send(&notify::test_event())
                    .context("sending the test notification")?;
                writeln!(out, "test notification sent via {names}")?;
            }
        }
        Ok(())
    }

    fn cmd_check(
        &mut self,
        action: Option<CheckAction>,
        opts: CheckOptions,
        out: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_layout()?;
        let lock = self.checks_lock()?;
        let now = Utc::now();
        let mut store = checks::load(&self.cfg.checks_file())?;
        let mut imported_created = 0usize;
        if !opts.no_sync {
            let imported = checks::sync_projects(&mut store, &self.cfg.projects_root_path(), now)?;
            imported_created = imported.created;
            if imported.total() > 0 {
                checks::save(&self.cfg.checks_file(), &store)?;
            }
        }
        if imported_created > 0 && !opts.watch {
            let items = checks::ordered_open(&store, now);
            let lines: Vec<String> = items
                .iter()
                .take(3)
                .map(|(check, _)| {
                    format!(
                        "{}: {} ({}, {})",
                        check.slug,
                        check.title,
                        checks::relative_label(check, now),
                        check.priority.as_str()
                    )
                })
                .collect();
            self.announce(
                Event::new(
                    EventKind::Checks,
                    format!("{imported_created} new human check(s)"),
                    lines.join("\n"),
                ),
                out,
            );
        }

        match action {
            Some(CheckAction::Add {
                title,
                project,
                due,
                kind,
                priority,
                detail,
                every,
                probe,
            }) => {
                if title.trim().is_empty() {
                    bail!("check title must not be empty");
                }
                let slug = self.resolve_project_slug(project)?;
                let kind: CheckKind = kind.parse().map_err(|err: String| anyhow::anyhow!(err))?;
                let priority: Priority = priority
                    .parse()
                    .map_err(|err: String| anyhow::anyhow!(err))?;
                let due_at = checks::parse_due(due.as_deref().unwrap_or(""), now)?;
                let mut check =
                    Check::new(&slug, title.trim(), kind, priority, due_at, detail, now);
                if let Some(every) = every {
                    check.every_secs = Some(checks::parse_span(&every)?);
                }
                check.probe = probe
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty());
                let id = store.add(check);
                checks::save(&self.cfg.checks_file(), &store)?;
                let _ = events::record(&self.cfg, "check", &slug, &format!("added {id}"));
                let added = store.get(&id).expect("just added");
                writeln!(
                    out,
                    "added {} [{}] {} — {} ({})",
                    id,
                    added.kind.as_str(),
                    added.title,
                    checks::relative_label(added, now),
                    added.priority.as_str()
                )?;
            }
            Some(CheckAction::Done { id, note }) => {
                store.mark_done(&id, &note, now)?;
                checks::save(&self.cfg.checks_file(), &store)?;
                let _ = events::record(&self.cfg, "check", "", &format!("done {id}"));
                let check = store.get(&id).expect("just marked done");
                writeln!(
                    out,
                    "done {} [{}] {}{}",
                    id,
                    check.kind.as_str(),
                    check.title,
                    if note.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", note.trim())
                    }
                )?;
            }
            Some(CheckAction::Later { id, when }) => {
                let due = checks::parse_due(&when, now)?;
                store.reschedule(&id, due, now)?;
                checks::save(&self.cfg.checks_file(), &store)?;
                let _ = events::record(&self.cfg, "check", "", &format!("later {id}"));
                let check = store.get(&id).expect("just rescheduled");
                writeln!(
                    out,
                    "rescheduled {} — {} ({})",
                    id,
                    check.title,
                    checks::relative_label(check, now)
                )?;
            }
            Some(CheckAction::Sync) => {
                // An explicit `sync` always imports, even with --no-sync.
                let outcome =
                    checks::sync_projects(&mut store, &self.cfg.projects_root_path(), now)?;
                checks::save(&self.cfg.checks_file(), &store)?;
                writeln!(
                    out,
                    "imported {} new check(s), {} refreshed ({} open total)",
                    outcome.created,
                    outcome.updated,
                    store.open().len()
                )?;
            }
            Some(CheckAction::Probe { slug }) => {
                let failures = self.probe_store(&mut store, slug.as_deref(), now, out)?;
                checks::save(&self.cfg.checks_file(), &store)?;
                writeln!(
                    out,
                    "probed {} check(s), {failures} failure(s)",
                    store.open().iter().filter(|c| c.probe.is_some()).count()
                )?;
            }
            Some(CheckAction::Remove { id }) => {
                store.remove(&id)?;
                checks::save(&self.cfg.checks_file(), &store)?;
                writeln!(out, "removed check {id}")?;
            }
            Some(CheckAction::Reopen { id }) => {
                store.reopen(&id, now)?;
                checks::save(&self.cfg.checks_file(), &store)?;
                writeln!(out, "reopened check {id}")?;
            }
            Some(CheckAction::Export { format }) => {
                if format == "json" {
                    writeln!(out, "{}", serde_json::to_string_pretty(&store)?)?;
                } else {
                    write!(out, "{}", checks::render(&store, now, true))?;
                }
            }
            Some(CheckAction::Daemon { interval }) => {
                drop(lock);
                return self.run_check_daemon(interval, out);
            }
            Some(CheckAction::Notify) => {
                if notify::channel_names(self.notifier.as_ref()) == "none" {
                    bail!("no notification channel configured; run `mox notify status` for setup");
                }
                let items = checks::ordered_open(&store, now);
                let mut overdue = 0usize;
                let mut now_count = 0usize;
                let mut lines = Vec::new();
                for (check, bucket) in items.iter().take(8) {
                    if *bucket == checks::Bucket::Overdue {
                        overdue += 1;
                    }
                    if *bucket == checks::Bucket::Now {
                        now_count += 1;
                    }
                    lines.push(format!(
                        "{}: {} ({}, {})",
                        check.slug,
                        check.title,
                        checks::relative_label(check, now),
                        check.priority.as_str()
                    ));
                }
                if overdue + now_count == 0 {
                    writeln!(
                        out,
                        "nothing is overdue or due now; no reminder sent ({} open check(s))",
                        store.open().len()
                    )?;
                    return Ok(());
                }
                let title = format!("{overdue} overdue, {now_count} due now");
                self.notifier
                    .send(&Event::new(EventKind::Digest, title, lines.join("\n")))
                    .context("sending the check reminder")?;
                writeln!(
                    out,
                    "reminder sent via {} ({} open check(s))",
                    notify::channel_names(self.notifier.as_ref()),
                    store.open().len()
                )?;
            }
            None => {
                drop(lock);
                if opts.watch {
                    return self.watch_checks(opts.interval, out);
                }
                if opts.json {
                    writeln!(
                        out,
                        "{}",
                        serde_json::to_string_pretty(&checks::views(&store, now))?
                    )?;
                } else if opts.oneline {
                    writeln!(out, "{}", checks::oneline(&store, now))?;
                } else {
                    write!(
                        out,
                        "{}",
                        checks::render_limited(&store, now, opts.all, opts.top)
                    )?;
                    if opts.context {
                        self.print_check_context(&store, now, opts.top, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Run probes in the store and mark failures due now.
    fn probe_store(
        &self,
        store: &mut CheckStore,
        slug: Option<&str>,
        now: chrono::DateTime<Utc>,
        out: &mut dyn Write,
    ) -> Result<usize> {
        let indices: Vec<usize> = store
            .checks
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.status == checks::CheckStatus::Open
                    && c.probe.is_some()
                    && slug.map(|s| c.slug == s).unwrap_or(true)
            })
            .map(|(i, _)| i)
            .collect();
        let mut failures = 0usize;
        for index in indices {
            let probe = store.checks[index].probe.clone().unwrap_or_default();
            let result = checks::run_probe(self.runner.as_ref(), &self.cfg, &probe);
            let check = &mut store.checks[index];
            check.probe_at = Some(now);
            match result {
                Ok(result) => {
                    check.probe_note = Some(format!(
                        "{}: {}",
                        if result.ok { "ok" } else { "FAILED" },
                        result.observed
                    ));
                    if !result.ok {
                        failures += 1;
                        check.due_at = Some(now);
                        if check.priority < checks::Priority::High {
                            check.priority = checks::Priority::High;
                        }
                    }
                    writeln!(
                        out,
                        "{} [{}] {} — {}",
                        check.id,
                        check.slug,
                        check.title,
                        check.probe_note.as_deref().unwrap_or("")
                    )?;
                }
                Err(err) => {
                    failures += 1;
                    check.probe_note = Some(format!("error: {err:#}"));
                    check.due_at = Some(now);
                    writeln!(out, "{} [{}] probe error: {err:#}", check.id, check.slug)?;
                }
            }
            check.updated_at = now;
        }
        Ok(failures)
    }

    fn print_check_context(
        &self,
        store: &CheckStore,
        now: chrono::DateTime<Utc>,
        top: Option<usize>,
        out: &mut dyn Write,
    ) -> Result<()> {
        let state = self.load_state()?;
        let mut commits: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut items = checks::ordered_open(store, now);
        if let Some(top) = top {
            items.truncate(top);
        }
        for (check, _) in items {
            let Some(project) = state.project(&check.slug) else {
                continue;
            };
            let commit = commits
                .entry(check.slug.clone())
                .or_insert_with(|| {
                    let output = self.runner.run(
                        &CommandSpec::new(&self.cfg.git_bin)
                            .args(["rev-parse", "--short", "HEAD"])
                            .cwd(&project.path)
                            .timeout(10),
                    );
                    output
                        .ok()
                        .filter(|o| o.success())
                        .map(|o| o.stdout.trim().to_string())
                        .unwrap_or_else(|| "-".to_string())
                })
                .clone();
            writeln!(
                out,
                "  {} {} @ {} ({})",
                check.id,
                display_path(Path::new(&project.path)),
                commit,
                check.slug
            )?;
        }
        Ok(())
    }

    /// Live sidebar. Reloads and re-imports on every tick so agent-written
    /// checks appear without restarting.
    fn watch_checks(&self, interval: u64, out: &mut dyn Write) -> Result<()> {
        let interval = interval.max(1);
        loop {
            let now = Utc::now();
            let mut store = checks::load(&self.cfg.checks_file())?;
            let _ = checks::sync_projects(&mut store, &self.cfg.projects_root_path(), now);
            write!(out, "\x1b[2J\x1b[H")?;
            write!(out, "{}", checks::render(&store, now, false))?;
            writeln!(out, "\nrefreshing every {interval}s — Ctrl-C to quit")?;
            out.flush()?;
            std::thread::sleep(std::time::Duration::from_secs(interval));
        }
    }

    /// Periodic reminder loop. Holds the checks lock only while syncing.
    fn run_check_daemon(&self, interval: u64, out: &mut dyn Write) -> Result<()> {
        let interval = interval.max(5);
        loop {
            if crate::shutdown::requested() {
                return Ok(());
            }
            let now = Utc::now();
            {
                let _lock = self.checks_lock()?;
                let mut store = checks::load(&self.cfg.checks_file())?;
                let _ = checks::sync_projects(&mut store, &self.cfg.projects_root_path(), now);
                checks::save(&self.cfg.checks_file(), &store)?;
            }
            let store = checks::load(&self.cfg.checks_file())?;
            let items = checks::ordered_open(&store, now);
            let due: Vec<&checks::Check> = items
                .iter()
                .filter(|(_, bucket)| {
                    matches!(bucket, checks::Bucket::Overdue | checks::Bucket::Now)
                })
                .map(|(check, _)| *check)
                .collect();
            if !due.is_empty() {
                let body = due
                    .iter()
                    .take(5)
                    .map(|c| {
                        format!(
                            "{}: {} ({})",
                            c.slug,
                            c.title,
                            checks::relative_label(c, now)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if notify::channel_names(self.notifier.as_ref()) != "none" {
                    let _ = self.notifier.send(&Event::new(
                        EventKind::Digest,
                        format!("{} check(s) due now", due.len()),
                        body,
                    ));
                }
                writeln!(
                    out,
                    "{}",
                    checks::render_limited(&store, now, false, Some(5))
                )?;
            }
            out.flush()?;
            let mut slept = 0u64;
            while slept < interval && !crate::shutdown::requested() {
                std::thread::sleep(std::time::Duration::from_secs(1));
                slept += 1;
            }
        }
    }

    fn resolve_project_slug(&self, requested: Option<String>) -> Result<String> {
        if let Some(slug) = requested {
            let state = self.load_state()?;
            if state.project(&slug).is_none() && !self.cfg.projects_root_path().join(&slug).is_dir()
            {
                let known: Vec<&str> = state.projects.keys().map(String::as_str).collect();
                bail!(
                    "unknown project `{slug}`{}",
                    if known.is_empty() {
                        String::new()
                    } else {
                        format!("; known: {}", known.join(", "))
                    }
                );
            }
            return Ok(slug);
        }
        let state = self.load_state()?;
        match state.projects.len() {
            0 => bail!("no projects yet; run `mox plan` then `mox up`"),
            1 => Ok(state.projects.keys().next().expect("one project").clone()),
            _ => bail!(
                "several projects exist; pass `--project <slug>` (known: {})",
                state
                    .projects
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// One-time human approval for remote deployment of one project.
    fn cmd_vps_allow(&mut self, slug: &str, note: &str, out: &mut dyn Write) -> Result<()> {
        self.ensure_layout()?;
        let mut state = self.load_state()?;
        let Some(mut project) = state.project(slug).cloned() else {
            bail!(
                "unknown project `{slug}`; known: {}",
                state
                    .projects
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        if self.dry_run {
            writeln!(out, "[dry-run] would allow VPS deployment for `{slug}`")?;
            return Ok(());
        }
        project.vps_approved_at = Some(now_rfc3339());
        project.vps_approved_note = note.trim().to_string();
        project.updated_at = now_rfc3339();
        state.upsert(project);
        self.save_state(&state)?;
        writeln!(
            out,
            "VPS deployment APPROVED for `{slug}` at {}.\n\
             This is persistent; revoke with `mox vps revoke {slug}`.\n\
             Reminder: agents still cannot ssh — remote work is executed by mox.",
            state
                .project(slug)
                .and_then(|p| p.vps_approved_at.clone())
                .unwrap_or_default()
        )?;
        if !note.trim().is_empty() {
            writeln!(out, "note: {}", note.trim())?;
        }
        Ok(())
    }

    fn cmd_vps_revoke(&mut self, slug: &str, out: &mut dyn Write) -> Result<()> {
        self.ensure_layout()?;
        let mut state = self.load_state()?;
        let Some(mut project) = state.project(slug).cloned() else {
            bail!("unknown project `{slug}`");
        };
        if project.vps_approved_at.is_none() {
            writeln!(out, "`{slug}` had no VPS approval")?;
            return Ok(());
        }
        if self.dry_run {
            writeln!(out, "[dry-run] would revoke VPS deployment for `{slug}`")?;
            return Ok(());
        }
        project.vps_approved_at = None;
        project.vps_approved_note = String::new();
        project.updated_at = now_rfc3339();
        state.upsert(project);
        self.save_state(&state)?;
        writeln!(out, "VPS deployment approval revoked for `{slug}`")?;
        Ok(())
    }

    fn cmd_vps_approvals(&mut self, out: &mut dyn Write) -> Result<()> {
        let state = self.load_state()?;
        let approved: Vec<&ProjectState> = state
            .ordered()
            .into_iter()
            .filter(|p| p.vps_approved_at.is_some())
            .collect();
        if approved.is_empty() {
            writeln!(out, "no project is approved for VPS deployment")?;
            return Ok(());
        }
        for project in approved {
            writeln!(
                out,
                "{:<28} approved {}  {}",
                project.slug,
                project.vps_approved_at.as_deref().unwrap_or(""),
                project.vps_approved_note
            )?;
        }
        Ok(())
    }

    fn cmd_vps_sync(
        &mut self,
        slug: Option<&str>,
        watch: bool,
        interval: u64,
        out: &mut dyn Write,
    ) -> Result<()> {
        loop {
            self.cmd_vps_sync_once(slug, out)?;
            if !watch || crate::shutdown::requested() {
                return Ok(());
            }
            out.flush()?;
            let mut slept = 0u64;
            while slept < interval.max(5) && !crate::shutdown::requested() {
                std::thread::sleep(std::time::Duration::from_secs(1));
                slept += 1;
            }
        }
    }

    fn cmd_vps_sync_once(&mut self, slug: Option<&str>, out: &mut dyn Write) -> Result<()> {
        self.ensure_layout()?;
        let _lock = self.up_lock()?;
        let mut state = self.load_state()?;
        let candidates: Vec<String> = state
            .ordered()
            .into_iter()
            .filter(|p| {
                p.target == "vps"
                    && p.status == ProjectStatus::Running
                    && p.remote_pid.is_some()
                    && slug.map(|s| s == p.slug).unwrap_or(true)
            })
            .map(|p| p.slug.clone())
            .collect();
        if candidates.is_empty() {
            writeln!(out, "no detached VPS run to sync")?;
            return Ok(());
        }
        for slug in candidates {
            let Some(mut project) = state.project(&slug).cloned() else {
                continue;
            };
            let dir = self.cfg.projects_root_path().join(&slug);
            match crate::remote::poll_detached(
                self.runner.as_ref(),
                &self.cfg,
                &self.cfg.ssh_alias,
                &slug,
            ) {
                Ok(crate::remote::DetachedState::Running) => {
                    let _ = self.runner.run(&crate::remote::rsync_board_spec(
                        &self.cfg,
                        &self.cfg.ssh_alias,
                        &slug,
                        &crate::milestones::board_path(&dir),
                    ));
                    let progress =
                        crate::milestones::load_optional(&crate::milestones::board_path(&dir))
                            .map(|board| crate::milestones::progress(&board, Utc::now()));
                    match progress {
                        Some(p) => writeln!(
                            out,
                            "{}: still running · {} · ETA {}",
                            slug,
                            p.short(),
                            p.compact_eta()
                        )?,
                        None => writeln!(out, "{slug}: still running")?,
                    }
                }
                Ok(crate::remote::DetachedState::Done { code }) => {
                    let log = self.cfg.logs_dir().join(format!("{slug}.remote.log"));
                    let outcome = crate::remote::finish_detached(
                        self.runner.as_ref(),
                        &self.cfg,
                        &slug,
                        &dir,
                        &self.cfg.ssh_alias,
                        code,
                        log.clone(),
                    )?;
                    project.remote_pid = None;
                    project.exit_code = code;
                    project.pid = None;
                    project.log_path = log.display().to_string();
                    let ok = code == Some(0) && outcome.merge_note.is_none();
                    if ok {
                        let verification =
                            crate::verify::verify(self.runner.as_ref(), &self.cfg, &dir);
                        if let Some(reason) = verification.reason() {
                            project.status = ProjectStatus::Failed;
                            project.last_error = Some(reason.clone());
                            writeln!(out, "{slug}: failed ({reason})")?;
                        } else {
                            project.status = ProjectStatus::Completed;
                            project.last_error = None;
                            writeln!(out, "{slug}: completed and fetched back")?;
                        }
                    } else {
                        project.status = ProjectStatus::Failed;
                        let reason = outcome.merge_note.clone().unwrap_or_else(|| {
                            if code == Some(124) {
                                format!("remote run timed out after {}s", self.cfg.run_timeout_secs)
                            } else {
                                format!("remote run exited with {code:?}")
                            }
                        });
                        project.last_error = Some(reason.clone());
                        writeln!(out, "{slug}: failed ({reason})")?;
                    }
                    let kind = if project.status == ProjectStatus::Completed {
                        EventKind::Finished
                    } else {
                        EventKind::Failed
                    };
                    self.announce(
                        Event::new(
                            kind,
                            format!("{slug}: {}", project.status.as_str()),
                            format!("remote log: {}", display_path(&log)),
                        ),
                        out,
                    );
                    let status = project.status.as_str().to_string();
                    state.upsert(project);
                    self.save_state(&state)?;
                    let _ = events::record(&self.cfg, "remote", &slug, &status);
                }
                Err(err) => {
                    writeln!(out, "{slug}: could not poll remote: {err:#}")?;
                }
            }
        }
        if self.cfg.recovery_enabled {
            let _ = crate::recovery::sync_all(self.runner.as_ref(), &self.cfg);
        }
        Ok(())
    }

    fn cmd_vps_ps(&mut self, out: &mut dyn Write) -> Result<()> {
        let state = self.load_state()?;
        let procs =
            crate::remote::audit(self.runner.as_ref(), &self.cfg, &self.cfg.ssh_alias, &state)?;
        writeln!(out, "{}", crate::remote::render_ps(&procs))?;
        if procs.iter().any(|p| p.is_bot()) {
            writeln!(
                out,
                "\nWARNING: a mox bot process is running on the VPS. mox never launches it; \
                 inspect it and use `mox vps cleanup` if it is not wanted."
            )?;
        }
        Ok(())
    }

    fn cmd_vps_cleanup(&mut self, apply: bool, yes: bool, out: &mut dyn Write) -> Result<()> {
        let state = self.load_state()?;
        let procs =
            crate::remote::audit(self.runner.as_ref(), &self.cfg, &self.cfg.ssh_alias, &state)?;
        let ghosts: Vec<&crate::remote::RemoteProcess> = procs
            .iter()
            .filter(|p| p.is_ghost() && (p.args.contains("mox-remote") || p.is_bot()))
            .collect();
        if ghosts.is_empty() {
            writeln!(out, "no untracked mox ghost to clean")?;
            return Ok(());
        }
        writeln!(out, "untracked mox processes:")?;
        for p in &ghosts {
            writeln!(
                out,
                "  pid {} · {} · {}",
                p.pid,
                crate::util::format_secs(p.etimes),
                p.args
            )?;
        }
        if !apply {
            writeln!(
                out,
                "\ndry run: pass `--apply --yes` to kill ONLY these pids."
            )?;
            return Ok(());
        }
        if !yes {
            bail!("refusing to kill remote processes without --yes");
        }
        let pids: Vec<u32> = ghosts.iter().map(|p| p.pid).collect();
        let output = self.runner.run(&crate::remote::cleanup_spec(
            &self.cfg,
            &self.cfg.ssh_alias,
            &pids,
        ))?;
        if !output.success() {
            bail!("cleanup failed: {}", output.tail(4));
        }
        writeln!(out, "sent SIGTERM/SIGKILL to {} ghost pid(s)", pids.len())?;
        Ok(())
    }

    fn cmd_vps_setup(&mut self, out: &mut dyn Write) -> Result<()> {
        let check = crate::remote::check(self.runner.as_ref(), &self.cfg, &self.cfg.ssh_alias);
        writeln!(out, "remote prerequisites for `{}`:", self.cfg.ssh_alias)?;
        writeln!(out, "  reachable: {}", check.reachable)?;
        writeln!(
            out,
            "  rsync:     {}",
            if check.rsync { "ok" } else { "MISSING" }
        )?;
        writeln!(
            out,
            "  git:       {}",
            if check.git { "ok" } else { "MISSING" }
        )?;
        writeln!(
            out,
            "  opencode:  {}",
            if check.opencode { "ok" } else { "MISSING" }
        )?;
        writeln!(out, "  disk free: {}", human_mb(check.disk_free_mb))?;
        if let Some(error) = &check.error {
            writeln!(out, "  error:     {error}")?;
        }
        if !check.ready() {
            writeln!(
                out,
                "\nmissing: {}.\n\
                 Install the missing tools on the VPS yourself (mox never installs software there),\n\
                 then re-run `mox vps setup`. No keys are copied.",
                check.missing().join(", ")
            )?;
            bail!("VPS is not ready for remote execution");
        }
        writeln!(out, "\nVPS is ready for `mox up --target vps`.")?;
        Ok(())
    }

    fn cmd_vps_plan(&mut self, compute: &str, out: &mut dyn Write) -> Result<()> {
        let need: ComputeNeed = compute
            .parse()
            .map_err(|err| anyhow::anyhow!("{err} (use light, medium, heavy or gpu)"))?;
        let probe = vps::probe(self.runner.as_ref(), &self.cfg.ssh_alias);
        let local = local_capacity_with_disk(self.runner.as_ref(), &self.cfg);
        let recommendation = vps::recommend(need, local, Some(&probe));
        writeln!(out, "OVHcloud remote plan checklist")?;
        writeln!(out, "workload: {}", need.as_str())?;
        writeln!(out, "target:   {}", recommendation.target.label())?;
        for reason in &recommendation.reasons {
            writeln!(out, "  - {reason}")?;
        }
        writeln!(
            out,
            "\n1. Order the suggested plan in the OVHcloud control panel (manual; billing)."
        )?;
        writeln!(
            out,
            "2. Choose Ubuntu 24.04 LTS and add your SSH key during creation."
        )?;
        writeln!(
            out,
            "3. Add an ssh alias (for example `mox-ovh`) to ~/.ssh/config, then set\n   ssh_alias = \"mox-ovh\" in the mox config.")?;
        writeln!(
            out,
            "4. Install `git`, `rsync` and `opencode` on it, then run `mox vps setup`."
        )?;
        writeln!(
            out,
            "5. Run `mox up --target vps --only <slug>` for approved projects."
        )?;
        writeln!(
            out,
            "\nmox never orders or provisions cloud resources automatically."
        )?;
        Ok(())
    }

    fn cmd_vps_check(&mut self, out: &mut dyn Write) -> Result<()> {
        let probe = vps::probe(self.runner.as_ref(), &self.cfg.ssh_alias);
        writeln!(out, "{}", vps::render_probe(&self.cfg.ssh_alias, &probe))?;
        let local = local_capacity_with_disk(self.runner.as_ref(), &self.cfg);
        writeln!(
            out,
            "local workstation: {} cores, {} RAM, {} free disk",
            local.cores,
            human_mb(local.mem_mb),
            human_mb(local.disk_free_mb)
        )?;
        writeln!(
            out,
            "\nread-only check; mox never starts, stops or edits VPS services."
        )?;
        Ok(())
    }

    fn cmd_vps_suggest(&mut self, compute: &str, out: &mut dyn Write) -> Result<()> {
        let need: ComputeNeed = compute
            .parse()
            .map_err(|err| anyhow::anyhow!("{err} (use light, medium, heavy or gpu)"))?;
        let probe = vps::probe(self.runner.as_ref(), &self.cfg.ssh_alias);
        let local = local_capacity_with_disk(self.runner.as_ref(), &self.cfg);
        let recommendation = vps::recommend(need, local, Some(&probe));
        writeln!(out, "workload: {}", need.as_str())?;
        writeln!(out, "target:   {}", recommendation.target.label())?;
        for reason in &recommendation.reasons {
            writeln!(out, "  - {reason}")?;
        }
        if let vps::ExecutionTarget::RemoteOvh { plan } = &recommendation.target {
            writeln!(
                out,
                "\nRemote plans are suggestions only: order/provision it yourself, then add an \
                 ssh alias and run `mox up` there.\nSuggested plan: {plan}"
            )?;
        }
        Ok(())
    }

    fn cmd_ideas(&mut self, file: &Path, out: &mut dyn Write) -> Result<()> {
        let ideas = ideas::load_ideas(file)?;
        writeln!(out, "{} idea(s):", ideas.len())?;
        for (index, idea) in ideas.iter().enumerate() {
            writeln!(
                out,
                "  {}. {}",
                index + 1,
                truncate_chars(idea.title.as_deref().unwrap_or(idea.text()), 70)
            )?;
        }
        Ok(())
    }
}

/// Check `/proc/<pid>/cmdline` before sending a signal. This prevents `mox stop`
/// from killing an unrelated process after pid reuse.
pub fn process_is_agent(pid: u32) -> bool {
    let path = PathBuf::from(format!("/proc/{pid}/cmdline"));
    match std::fs::read(path) {
        Ok(bytes) => {
            let cmdline = String::from_utf8_lossy(&bytes).replace('\0', " ");
            cmdline.contains("opencode")
        }
        Err(_) => false,
    }
}

fn local_capacity_with_disk(runner: &dyn CommandRunner, cfg: &Config) -> HostCapacity {
    let mut capacity = vps::local_capacity();
    let probe = DfProbe::new(runner);
    if let Ok(free) = probe.free_mb(&cfg.projects_root_path()) {
        capacity.disk_free_mb = free;
    }
    capacity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::ScriptedBrain;
    use crate::exec::{FakeRunner, Output};
    use crate::model::{Idea, Question};
    use std::sync::Mutex;

    fn temp_app(root: &Path) -> App {
        let cfg = Config {
            projects_root: root.join("projects").to_string_lossy().to_string(),
            state_dir: root.join("state").to_string_lossy().to_string(),
            max_parallel: 2,
            recovery_enabled: false,
            ..Config::default()
        };
        let runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "df" {
                return Output {
                    code: Some(0),
                    stdout: "Filesystem 1048576-blocks Used Available Capacity Mounted on\n\
                             /dev/x 100000 50000 50000 50% /\n"
                        .into(),
                    ..Output::default()
                };
            }
            Output::ok()
        }));
        App::new(
            cfg,
            root.join("config.toml"),
            runner,
            Arc::new(ScriptedBrain::default()),
            false,
            false,
        )
    }

    fn runner_that_publishes() -> FakeRunner {
        let created = Arc::new(Mutex::new(false));
        FakeRunner::new(move |spec| {
            if spec.program == "df" {
                return Output {
                    code: Some(0),
                    stdout: "Filesystem 1048576-blocks Used Available Capacity Mounted on\n/dev/x 100 50 50000 50% /\n".into(),
                    ..Output::default()
                };
            }
            if spec.program == "gh" && spec.args.contains(&"create".to_string()) {
                *created.lock().unwrap() = true;
            }
            if spec.program == "gh" && spec.args.contains(&"view".to_string()) {
                return Output {
                    code: Some(0),
                    stdout: r#"{"visibility":"PRIVATE","url":"https://github.com/me/demo"}"#.into(),
                    ..Output::default()
                };
            }
            if spec.program == "git" && spec.args.contains(&"get-url".to_string()) {
                return Output {
                    code: Some(0),
                    stdout: "git@github.com:me/demo.git\n".into(),
                    ..Output::default()
                };
            }
            Output::ok()
        })
    }

    fn write_plan(app: &App, specs: Vec<ProjectSpec>) {
        let plan = Plan {
            created_at: now_rfc3339(),
            source: "test".into(),
            source_hash: None,
            host: None,
            specs,
        };
        state::save_plan(&app.cfg.plan_file(), &plan).unwrap();
    }

    fn spec(slug: &str) -> ProjectSpec {
        let idea = Idea {
            title: Some(slug.into()),
            details: "build it".into(),
            raw: String::new(),
        };
        let mut spec = ProjectSpec::from_idea(&idea, slug.into());
        spec.questions.push(Question {
            id: "q1".into(),
            question: "Which storage?".into(),
            why: None,
            default: Some("SQLite".into()),
        });
        spec
    }

    #[test]
    fn plan_falls_back_without_ideas_file_when_cached() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_plan(
            PlanOptions {
                yes: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("demo"));
        assert!(app.cfg.answers_file().exists());
        assert!(app.cfg.questions_file().exists());
    }

    #[test]
    fn plan_saves_state_and_questions() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.brain = Arc::new(ScriptedBrain::new(vec![
            r#"{"projects":[{"slug":"expense","title":"Expense tracker","summary":"s",
                "questions":[{"id":"q1","question":"Which DB?","default":"SQLite"}],
                "compute":"medium"}]}"#
                .to_string(),
        ]));
        let ideas = dir.path().join("ideas.txt");
        std::fs::write(&ideas, "- Expense tracker :: track my expenses\n").unwrap();
        let mut out = Vec::new();
        app.cmd_plan(
            PlanOptions {
                ideas: Some(ideas),
                yes: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("expense"));
        assert!(text.contains("estimates for:"), "{text}");
        assert!(text.contains("difficulty"), "{text}");
        assert!(text.contains("~220k"), "{text}");
        let saved = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        assert_eq!(saved.specs.len(), 1);
        assert_eq!(saved.specs[0].compute, ComputeNeed::Medium);
        assert_eq!(saved.specs[0].difficulty, crate::model::Complexity::High);
        assert_eq!(saved.specs[0].estimated_tokens, 220_000);
        assert_eq!(saved.specs[0].token_estimate_model, app.cfg.estimate_model);
        let host = saved.host.as_ref().expect("host profile saved");
        assert!(host.cores >= 1);
        assert!(!host.cpu_model.is_empty());
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.projects["expense"].status, ProjectStatus::Planned);
        let questions = std::fs::read_to_string(app.cfg.questions_file()).unwrap();
        assert!(questions.contains("Which DB?"));
    }

    #[test]
    fn plan_deep_refines_the_spec_from_answers() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec);
        let brain = Arc::new(ScriptedBrain::new(vec![
            r#"{"projects":[{"slug":"expense","title":"Expense","summary":"old",
                "questions":[{"id":"q1","question":"Which DB?","default":"SQLite"}]}]}"#
                .to_string(),
            r#"{"projects":[{"slug":"expense","summary":"refined summary",
                "difficulty":"high","estimated_tokens":777000,
                "acceptance_criteria":["works end to end"],"risks":["scope"],
                "assumptions":["single user"]}]}"#
                .to_string(),
        ]));
        app.brain = brain.clone();
        let ideas = dir.path().join("ideas.txt");
        std::fs::write(&ideas, "- Expense\n").unwrap();
        let mut out = Vec::new();
        app.cmd_plan(
            PlanOptions {
                ideas: Some(ideas),
                yes: true,
                deep: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let saved = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        assert_eq!(saved.specs[0].summary, "refined summary");
        assert_eq!(saved.specs[0].acceptance_criteria, vec!["works end to end"]);
        assert_eq!(saved.specs[0].risks, vec!["scope"]);
        assert_eq!(saved.specs[0].difficulty, crate::model::Complexity::High);
        assert_eq!(saved.specs[0].estimated_tokens, 777_000);
        assert_eq!(brain.prompts().len(), 2);
    }

    #[test]
    fn plan_applies_existing_answers_and_asks_interactively() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.interactive = true;
        app.asker = Box::new(ScriptedAsker::new(vec![Some("PostgreSQL".to_string())]));
        app.brain = Arc::new(ScriptedBrain::new(vec![
            r#"{"projects":[{"slug":"expense","title":"Expense","questions":[
                {"id":"q1","question":"Which DB?","default":"SQLite"},
                {"id":"q2","question":"Which UI?","default":"CLI"}]}]}"#
                .to_string(),
        ]));
        let ideas = dir.path().join("ideas.txt");
        std::fs::write(&ideas, "- Expense\n").unwrap();
        let mut out = Vec::new();
        app.cmd_plan(
            PlanOptions {
                ideas: Some(ideas),
                interactive: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let saved = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        assert_eq!(saved.specs[0].answers["q1"], "PostgreSQL");
        // q2 had no scripted answer but a default, so the default was used.
        assert_eq!(saved.specs[0].answers["q2"], "CLI");
    }

    fn remote_runner() -> FakeRunner {
        FakeRunner::new(|spec| {
            if spec.program == "df" {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "x\n/dev/x 1 1 50000 1% /\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "ssh" {
                let command = spec.args.last().cloned().unwrap_or_default();
                if command.contains("OPENCODE") {
                    return crate::exec::Output {
                        code: Some(0),
                        stdout: "RSYNC=/usr/bin/rsync\nGIT=/usr/bin/git\nOPENCODE=/usr/bin/opencode\nDISK_MB=30000\n".into(),
                        ..crate::exec::Output::default()
                    };
                }
                if command.contains("setsid") {
                    return crate::exec::Output {
                        code: Some(0),
                        stdout: "4242\n".into(),
                        ..crate::exec::Output::default()
                    };
                }
                if command.contains("STATE=RUNNING") || command.contains("remote.exit") {
                    return crate::exec::Output {
                        code: Some(0),
                        stdout: "STATE=DONE EXIT=0\n".into(),
                        ..crate::exec::Output::default()
                    };
                }
                return crate::exec::Output::ok();
            }
            if spec.program == "git" && spec.args.contains(&"status".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: String::new(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "git" && spec.args.contains(&"@{u}".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "origin/main\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "git" && spec.args.contains(&"rev-list".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "0\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            crate::exec::Output::ok()
        })
    }

    #[test]
    fn up_target_vps_requires_approval() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(remote_runner());
        state_with_demo(&app);
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                no_github: true,
                target: "vps".into(),
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("not approved"), "{text}");
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].status, ProjectStatus::Failed);
        assert!(state.projects["demo"]
            .last_error
            .as_deref()
            .unwrap()
            .contains("approved"));
    }

    #[test]
    fn up_target_vps_runs_remotely_and_fetches_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(remote_runner());
        state_with_demo(&app);
        let mut out = Vec::new();
        app.cmd_vps_allow("demo", "remote ok", &mut out).unwrap();
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                no_github: true,
                target: "vps".into(),
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("VPS"), "{text}");
        let state = state::load(&app.cfg.state_file()).unwrap();
        let project = state.project("demo").unwrap();
        assert_eq!(project.target, "vps");
        assert_eq!(project.status, ProjectStatus::Completed);
        let displays = app.runner_displays();
        assert!(displays.iter().any(|d| d.starts_with("rsync -az")));
        assert!(displays.iter().any(|d| d.contains("ssh")));
        assert!(displays.iter().any(|d| d.contains("git fetch")));
    }

    #[test]
    fn detached_vps_run_survives_and_syncs_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(remote_runner());
        state_with_demo(&app);
        let mut out = Vec::new();
        app.cmd_vps_allow("demo", "detached ok", &mut out).unwrap();
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                no_github: true,
                target: "vps".into(),
                detach: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        let project = state.project("demo").unwrap();
        assert_eq!(project.status, ProjectStatus::Running);
        assert_eq!(project.remote_pid, Some(4242));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("detached on VPS"), "{text}");

        let mut out = Vec::new();
        app.cmd_vps_sync(None, false, 60, &mut out).unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        let project = state.project("demo").unwrap();
        assert_eq!(project.status, ProjectStatus::Completed);
        assert_eq!(project.remote_pid, None);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed and fetched back"), "{text}");
    }

    #[test]
    fn vps_cleanup_only_kills_untracked_mox_processes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "ssh" {
                let command = spec.args.last().cloned().unwrap_or_default();
                if command.contains("ps -eo") {
                    return crate::exec::Output {
                        code: Some(0),
                        stdout: "100 1 50 /home/u/mox-remote/demo/.mox/run.sh\n\
                                 200 1 7 /home/u/mox-remote/old/.mox/run.sh\n\
                                 300 1 3 mox bot --once\n"
                            .into(),
                        ..crate::exec::Output::default()
                    };
                }
            }
            crate::exec::Output::ok()
        }));
        let mut state = State::default();
        let mut tracked = ProjectState::new("demo", "Demo", "/tmp/demo");
        tracked.target = "vps".into();
        tracked.status = ProjectStatus::Running;
        tracked.remote_pid = Some(100);
        state.upsert(tracked);
        app.save_state(&state).unwrap();

        let mut out = Vec::new();
        app.cmd_vps_cleanup(false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("200"), "{text}");
        assert!(text.contains("300"), "{text}");
        assert!(!text.contains("pid 100"), "{text}");
        assert!(text.contains("dry run"), "{text}");

        let mut out = Vec::new();
        app.cmd_vps_cleanup(true, true, &mut out).unwrap();
        let displays = app.runner_displays();
        let cleanup = displays
            .iter()
            .find(|d| d.contains("kill -TERM"))
            .expect("cleanup command");
        assert!(cleanup.contains("200"), "{cleanup}");
        assert!(cleanup.contains("300"), "{cleanup}");
        assert!(!cleanup.contains(" 100 "), "{cleanup}");
    }

    #[test]
    fn vps_setup_reports_missing_opencode() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "ssh" {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "RSYNC=/usr/bin/rsync\nGIT=/usr/bin/git\nOPENCODE=\nDISK_MB=30000\n"
                        .into(),
                    ..crate::exec::Output::default()
                };
            }
            crate::exec::Output::ok()
        }));
        let mut out = Vec::new();
        let err = app.cmd_vps_setup(&mut out).unwrap_err();
        assert!(err.to_string().contains("not ready"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("opencode:  MISSING"), "{text}");
    }

    #[test]
    fn up_resume_skips_completed_projects() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("done"), spec("todo")]);
        let mut state = State::default();
        let mut done = ProjectState::new("done", "done", "/tmp/done");
        done.status = ProjectStatus::Completed;
        state.upsert(done);
        state.upsert(ProjectState::new("todo", "todo", "/tmp/todo"));
        app.save_state(&state).unwrap();

        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                resume: true,
                no_github: true,
                no_run: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("preparing 1 project"), "{text}");
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["done"].status, ProjectStatus::Completed);
        assert_eq!(state.projects["todo"].status, ProjectStatus::Scaffolded);
    }

    #[test]
    fn up_retry_failed_only_touches_failed_projects() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("broken"), spec("done")]);
        let mut state = State::default();
        let mut broken = ProjectState::new("broken", "broken", "/tmp/broken");
        broken.status = ProjectStatus::Failed;
        state.upsert(broken);
        let mut done = ProjectState::new("done", "done", "/tmp/done");
        done.status = ProjectStatus::Completed;
        state.upsert(done);
        app.save_state(&state).unwrap();

        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                retry_failed: true,
                no_github: true,
                no_run: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("preparing 1 project"), "{text}");
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["broken"].status, ProjectStatus::Scaffolded);
        assert_eq!(state.projects["done"].status, ProjectStatus::Completed);
    }

    #[test]
    fn up_dry_run_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.dry_run = true;
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: false,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[dry-run]"));
        assert!(text.contains("no state written"));
        assert!(!app.cfg.projects_root_path().join("demo").exists());
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert!(state.projects.is_empty(), "dry run must not write state");
    }

    #[test]
    fn up_scaffolds_publishes_and_runs_with_private_repo() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(runner_that_publishes());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: Some(1),
                no_github: false,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("private repo: https://github.com/me/demo"));
        let project = app.cfg.projects_root_path().join("demo");
        assert!(project.join("TASK.md").exists());
        assert!(project.join(".gitignore").exists());
        assert!(app.cfg.logs_dir().join("demo.log").exists());
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].status, ProjectStatus::Completed);
        assert_eq!(
            state.projects["demo"].repo_url,
            "https://github.com/me/demo"
        );
        assert!(!state.projects["demo"].cores.is_empty());

        let displays = app.runner_displays();
        assert!(displays
            .iter()
            .any(|d| d.contains("repo create demo --private")));
        assert!(displays.iter().any(|d| d.contains("opencode run --auto")));
        assert!(!displays.iter().any(|d| d.contains("--public")));
    }

    #[test]
    fn up_marks_dirty_work_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.cfg.taskset_bin = String::new();
        app.cfg.nice_bin = String::new();
        app.cfg.ionice_bin = String::new();
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "df" {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "x\n/dev/x 1 1 50000 1% /\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "git" && spec.args.contains(&"status".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: " M src/main.rs\n?? notes.txt\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "git" && spec.args.contains(&"@{u}".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "origin/main\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "git" && spec.args.contains(&"rev-list".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "0\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            crate::exec::Output::ok()
        }));
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: true,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        let project = state.project("demo").unwrap();
        assert_eq!(project.status, ProjectStatus::Failed);
        assert!(
            project
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("2 uncommitted"),
            "{:?}",
            project.last_error
        );
    }

    #[test]
    fn up_marks_a_timed_out_run_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.cfg.taskset_bin = String::new();
        app.cfg.nice_bin = String::new();
        app.cfg.ionice_bin = String::new();
        app.cfg.run_timeout_secs = 42;
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "df" {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "x\n/dev/x 1 1 50000 1% /\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "opencode" {
                return crate::exec::Output {
                    code: Some(124),
                    timed_out: true,
                    ..crate::exec::Output::default()
                };
            }
            crate::exec::Output::ok()
        }));
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: true,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        let project = state.project("demo").unwrap();
        assert_eq!(project.status, ProjectStatus::Failed);
        assert_eq!(project.exit_code, Some(124));
        assert!(
            project
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after 42s"),
            "{:?}",
            project.last_error
        );
    }

    #[test]
    fn up_skips_github_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: Some(1),
                no_github: true,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].repo_url, "");
        let gh_calls: Vec<_> = app
            .runner
            .recorded_calls()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.program == app.cfg.gh_bin)
            .collect();
        assert!(gh_calls.is_empty(), "no GitHub call expected: {gh_calls:?}");
        let displays = app.runner_displays();
        assert!(displays.iter().any(|d| d.contains("opencode run")));
    }

    #[test]
    fn up_rejects_unknown_only_slug() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        let err = app
            .cmd_up(
                UpOptions {
                    only: vec!["nope".into()],
                    max_parallel: None,
                    no_github: true,
                    no_run: true,
                    force: false,
                    ..Default::default()
                },
                &mut out,
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown project"));
    }

    #[test]
    fn up_marks_failed_when_repo_creation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "df" {
                return Output {
                    code: Some(0),
                    stdout: "Filesystem 1048576-blocks Used Available Capacity Mounted on\n/dev/x 1 1 50000 50% /\n".into(),
                    ..Output::default()
                };
            }
            if spec.program == "gh" && spec.args.contains(&"create".to_string()) {
                return Output::failure(1, "Name already exists");
            }
            Output::ok()
        }));
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: Some(1),
                no_github: false,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].status, ProjectStatus::Failed);
        assert!(state.projects["demo"]
            .last_error
            .as_deref()
            .unwrap()
            .contains("already exists"));
    }

    #[test]
    fn status_json_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        let mut state = State::default();
        state.upsert(ProjectState::new("demo", "Demo", "/tmp/nope"));
        app.save_state(&state).unwrap();
        let mut out = Vec::new();
        app.cmd_status(false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("slug"));
        assert!(text.contains("demo"));
        let mut json_out = Vec::new();
        app.cmd_status(true, false, &mut json_out).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(value["projects"]["demo"]["title"], "Demo");
    }

    #[test]
    fn status_shows_milestone_progress_and_eta() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let project_dir = app.cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project_dir).unwrap();

        let mut spec = spec("demo");
        spec.milestones = crate::milestones::fallback_for(&spec);
        let mut board = crate::milestones::board_from_spec(&spec, Utc::now());
        crate::milestones::start(&mut board, "m1", Utc::now()).unwrap();
        crate::milestones::done(
            &mut board,
            "m1",
            "",
            Utc::now() + chrono::Duration::minutes(5),
        )
        .unwrap();
        crate::milestones::start(&mut board, "m2", Utc::now()).unwrap();
        crate::milestones::save(&crate::milestones::board_path(&project_dir), &board).unwrap();

        let mut state = State::default();
        state.upsert(ProjectState::new(
            "demo",
            "Demo",
            &project_dir.display().to_string(),
        ));
        app.save_state(&state).unwrap();

        let mut out = Vec::new();
        app.cmd_status(false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("progress"), "{text}");
        assert!(text.contains("m2/3"), "{text}");
        assert!(text.contains("33%"), "{text}");

        let mut json_out = Vec::new();
        app.cmd_status(true, false, &mut json_out).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(value["milestone_progress"]["demo"]["current"], "m2");
        assert_eq!(value["milestone_progress"]["demo"]["percent"], 33);
        assert_eq!(value["milestone_progress"]["demo"]["done"], 1);
    }

    #[test]
    fn canceled_run_is_marked_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.cfg.taskset_bin = String::new();
        app.cfg.nice_bin = String::new();
        app.cfg.ionice_bin = String::new();
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "df" {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "x\n/dev/x 1 1 50000 1% /\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "opencode" {
                return crate::exec::Output {
                    cancelled: true,
                    ..crate::exec::Output::default()
                };
            }
            crate::exec::Output::ok()
        }));
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: true,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].status, ProjectStatus::Stopped);
        assert!(state.projects["demo"]
            .last_error
            .as_deref()
            .unwrap()
            .contains("cancelled"));
    }

    #[test]
    fn status_marks_stale_running_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        let mut state = State::default();
        let mut project = ProjectState::new("demo", "Demo", "/tmp/nope");
        project.status = ProjectStatus::Running;
        state.upsert(project);
        app.save_state(&state).unwrap();
        let mut out = Vec::new();
        app.cmd_status(false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("stale"), "{text}");
        assert!(text.contains("mox up --resume"), "{text}");
    }

    #[test]
    fn status_verbose_and_report_include_project_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        let project_dir = app.cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("STATUS.md"),
            "# Status\n\nWorking: parser\nNext: tests\n",
        )
        .unwrap();
        let mut state = State::default();
        state.upsert(ProjectState::new(
            "demo",
            "Demo",
            &project_dir.display().to_string(),
        ));
        app.save_state(&state).unwrap();

        let mut out = Vec::new();
        app.cmd_status(false, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Working: parser"), "{text}");
        assert!(text.contains("Next: tests"), "{text}");

        let mut report = Vec::new();
        app.cmd_report(None, &mut report).unwrap();
        let report = String::from_utf8(report).unwrap();
        assert!(report.contains("# mox report"), "{report}");
        assert!(report.contains("| demo |"), "{report}");
        assert!(report.contains("Working: parser"), "{report}");
        assert!(report.contains("Timing statistics"), "{report}");

        let path = dir.path().join("report.md");
        let mut out = Vec::new();
        app.cmd_report(Some(&path), &mut out).unwrap();
        assert!(path.exists());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("mox report"));
    }

    #[test]
    fn logs_reports_missing_then_tails() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let mut out = Vec::new();
        app.cmd_logs("demo", 5, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("no log"));
        std::fs::write(app.cfg.logs_dir().join("demo.log"), "a\nb\nc\n").unwrap();
        let mut out = Vec::new();
        app.cmd_logs("demo", 2, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "b\nc\n");
    }

    fn call_check(
        app: &mut App,
        action: Option<CheckAction>,
        json: bool,
        oneline: bool,
        all: bool,
        no_sync: bool,
    ) -> (String, Result<()>) {
        let mut out = Vec::new();
        let result = app.cmd_check(
            action,
            CheckOptions {
                json,
                oneline,
                watch: false,
                interval: 10,
                all,
                no_sync,
                top: None,
                context: false,
            },
            &mut out,
        );
        (String::from_utf8(out).unwrap(), result)
    }

    fn state_with_demo(app: &App) {
        let mut state = State::default();
        state.upsert(ProjectState::new(
            "demo",
            "Demo",
            &app.cfg
                .projects_root_path()
                .join("demo")
                .display()
                .to_string(),
        ));
        app.save_state(&state).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_layout_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        for path in [
            app.cfg.state_dir_path(),
            app.cfg.internal_dir(),
            app.cfg.logs_dir(),
            app.cfg.run_dir(),
            app.cfg.cache_dir(),
            app.cfg.tmp_dir(),
        ] {
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} must be 0700",
                path.display()
            );
        }

        write_plan(&app, vec![spec("demo")]);
        assert_eq!(
            std::fs::metadata(app.cfg.plan_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        app.save_state(&State::default()).unwrap();
        assert_eq!(
            std::fs::metadata(app.cfg.state_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut store = checks::CheckStore::default();
        store.add(Check::new(
            "demo",
            "x",
            CheckKind::Manual,
            Priority::Normal,
            None,
            "",
            Utc::now(),
        ));
        checks::save(&app.cfg.checks_file(), &store).unwrap();
        assert_eq!(
            std::fs::metadata(app.cfg.checks_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        app.cmd_up(
            UpOptions {
                no_github: true,
                ..Default::default()
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(app.cfg.logs_dir().join("demo.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn check_add_list_later_and_done() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);

        let (out, result) = call_check(
            &mut app,
            Some(CheckAction::Add {
                title: "Verify the fetcher cron ran".into(),
                project: None,
                due: Some("+2h".into()),
                kind: "vps".into(),
                priority: "high".into(),
                detail: "check systemctl and logs".into(),
                every: None,
                probe: None,
            }),
            false,
            false,
            false,
            true,
        );
        result.unwrap();
        assert!(out.contains("added "), "{out}");
        let id = out.split_whitespace().nth(1).unwrap().to_string();

        let (list, _) = call_check(&mut app, None, false, false, false, true);
        assert!(list.contains("NOW (1)"), "{list}");
        assert!(list.contains("Verify the fetcher cron ran"));
        assert!(list.contains("mox check done <id>"));

        let (out, _) = call_check(
            &mut app,
            Some(CheckAction::Later {
                id: id.clone(),
                when: "+3d".into(),
            }),
            false,
            false,
            false,
            true,
        );
        assert!(out.contains("rescheduled"), "{out}");
        let (list, _) = call_check(&mut app, None, false, false, false, true);
        assert!(list.contains("SOON (1)"), "{list}");

        let (out, _) = call_check(
            &mut app,
            Some(CheckAction::Done {
                id: id.clone(),
                note: "looked fine".into(),
            }),
            false,
            false,
            false,
            true,
        );
        assert!(out.contains("looked fine"), "{out}");
        let (list, _) = call_check(&mut app, None, false, false, false, true);
        assert!(list.contains("nothing to check"), "{list}");
        let (with_done, _) = call_check(&mut app, None, false, false, true, true);
        assert!(with_done.contains("DONE (1)"), "{with_done}");
    }

    #[test]
    fn check_sync_imports_agent_file_and_reports_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let project = app.cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("CHECKS.md"),
            "- [ ] Check API quota | due: +1d | kind: api | priority: normal\n",
        )
        .unwrap();

        let (json, _) = call_check(&mut app, None, true, false, false, false);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["title"], "Check API quota");
        assert_eq!(value[0]["bucket"], "today");
        assert_eq!(value[0]["kind"], "api");
        assert!(app.cfg.checks_file().exists());

        let (line, _) = call_check(&mut app, None, false, true, false, true);
        assert!(line.starts_with("checks: "), "{line}");

        let (sync, _) = call_check(&mut app, Some(CheckAction::Sync), false, false, false, true);
        assert!(sync.contains("0 new check(s), 1 refreshed"), "{sync}");
    }

    #[test]
    fn check_probe_and_context_are_wired() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let mut store = checks::CheckStore::default();
        let id = store.add(
            Check::new(
                "demo",
                "Verify cron",
                CheckKind::Vps,
                Priority::Normal,
                Some(Utc::now()),
                "",
                Utc::now(),
            )
            .with_probe(Some("systemctl is-active fetcher".into())),
        );
        checks::save(&app.cfg.checks_file(), &store).unwrap();

        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "ssh" {
                let command = spec.args.last().cloned().unwrap_or_default();
                if command.contains("systemctl is-active") {
                    return crate::exec::Output {
                        code: Some(0),
                        stdout: "active\n".into(),
                        ..crate::exec::Output::default()
                    };
                }
            }
            if spec.program == "git" && spec.args.contains(&"rev-parse".to_string()) {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "abc1234\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            crate::exec::Output::ok()
        }));
        let mut out = Vec::new();
        app.cmd_check(
            Some(CheckAction::Probe { slug: None }),
            CheckOptions {
                no_sync: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("probed 1 check"), "{text}");
        let stored = checks::load(&app.cfg.checks_file()).unwrap();
        assert!(stored
            .get(&id)
            .unwrap()
            .probe_note
            .as_deref()
            .unwrap()
            .contains("ok"));

        let mut out = Vec::new();
        app.cmd_check(
            None,
            CheckOptions {
                no_sync: true,
                context: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("abc1234"), "{text}");
        assert!(text.contains("demo"), "{text}");
    }

    #[test]
    fn check_remove_reopen_export_and_top() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let mut store = checks::CheckStore::default();
        let first = store.add(Check::new(
            "demo",
            "First check",
            CheckKind::Manual,
            Priority::High,
            Some(Utc::now() - chrono::Duration::minutes(1)),
            "",
            Utc::now(),
        ));
        store.add(Check::new(
            "demo",
            "Second check",
            CheckKind::Manual,
            Priority::Normal,
            Some(Utc::now() + chrono::Duration::hours(2)),
            "",
            Utc::now(),
        ));
        checks::save(&app.cfg.checks_file(), &store).unwrap();

        // --top 1 shows only the first (most urgent) check.
        let mut out = Vec::new();
        app.cmd_check(
            None,
            CheckOptions {
                top: Some(1),
                no_sync: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("First check"), "{text}");
        assert!(!text.contains("Second check"), "{text}");

        // Export markdown and json.
        let mut md = Vec::new();
        app.cmd_check(
            Some(CheckAction::Export {
                format: "md".into(),
            }),
            CheckOptions {
                no_sync: true,
                ..Default::default()
            },
            &mut md,
        )
        .unwrap();
        assert!(String::from_utf8(md).unwrap().contains("First check"));
        let mut json = Vec::new();
        app.cmd_check(
            Some(CheckAction::Export {
                format: "json".into(),
            }),
            CheckOptions {
                no_sync: true,
                ..Default::default()
            },
            &mut json,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["checks"].as_array().unwrap().len(), 2);

        // Done -> reopen -> remove.
        let (_, _) = call_check(
            &mut app,
            Some(CheckAction::Done {
                id: first.clone(),
                note: "ok".into(),
            }),
            false,
            false,
            false,
            true,
        );
        let (_, _) = call_check(
            &mut app,
            Some(CheckAction::Reopen { id: first.clone() }),
            false,
            false,
            false,
            true,
        );
        assert_eq!(
            checks::load(&app.cfg.checks_file())
                .unwrap()
                .get(&first)
                .unwrap()
                .status,
            checks::CheckStatus::Open
        );
        let (_, _) = call_check(
            &mut app,
            Some(CheckAction::Remove { id: first.clone() }),
            false,
            false,
            false,
            true,
        );
        assert!(checks::load(&app.cfg.checks_file())
            .unwrap()
            .get(&first)
            .is_none());
    }

    #[test]
    fn project_adopt_registers_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let path = app.cfg.projects_root_path().join("legacy");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("keep.txt"), "x").unwrap();
        let mut out = Vec::new();
        app.cmd_project_adopt(&path, None, &mut out).unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert!(state.project("legacy").is_some());
        assert!(path.join("MILESTONES.json").exists());
        assert!(path.join("TASKS.md").exists());
        let plan = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        let spec = plan.get("legacy").expect("adopted spec");
        assert_eq!(spec.difficulty, crate::model::Complexity::High);
        assert_eq!(spec.estimated_tokens, 220_000);
        assert_eq!(spec.token_estimate_model, app.cfg.estimate_model);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("adopted legacy"));
        assert!(out.contains("estimate: high"));
        assert!(out.contains("~220k"));
    }

    #[test]
    fn project_archive_moves_the_workspace_without_deleting_anything() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let project = app.cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("keep.txt"), "x").unwrap();
        let mut state = State::default();
        state.upsert(ProjectState::new(
            "demo",
            "Demo",
            &project.display().to_string(),
        ));
        app.save_state(&state).unwrap();

        let mut out = Vec::new();
        app.cmd_project_archive("demo", &mut out).unwrap();
        assert!(!project.exists());
        let state = state::load(&app.cfg.state_file()).unwrap();
        let archived = state.project("demo").unwrap();
        assert_eq!(archived.status, ProjectStatus::Archived);
        let moved = PathBuf::from(&archived.path);
        assert!(moved.is_dir());
        assert!(moved.join("keep.txt").exists());
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("GitHub repository is untouched"));
    }

    #[test]
    fn init_profiles_and_completions() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        let mut out = Vec::new();
        app.cmd_init(false, "paranoid", &mut out).unwrap();
        let cfg = Config::load(Some(&app.cfg_path)).unwrap();
        assert_eq!(cfg.max_parallel, 1);
        assert!(!cfg.auto_approve);
        assert_eq!(cfg.min_free_disk_mb, 4096);

        let mut out = Vec::new();
        app.cmd_completions("bash", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("complete -F _mox mox"), "{text}");
        let mut out = Vec::new();
        app.cmd_completions("fish", &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("complete -c mox"));
    }

    #[test]
    fn events_command_and_log_retention() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        events::record(&app.cfg, "test", "demo", "hello").unwrap();
        let mut out = Vec::new();
        app.cmd_events(5, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("hello"));

        let log = app.cfg.logs_dir().join("old.log");
        std::fs::write(&log, "old").unwrap();
        let status = std::process::Command::new("touch")
            .args(["-d", "40 days ago"])
            .arg(&log)
            .status()
            .unwrap();
        assert!(status.success());
        let mut out = Vec::new();
        app.cmd_clean(false, false, false, Some(30), &mut out)
            .unwrap();
        assert!(log.exists());
        assert!(String::from_utf8(out).unwrap().contains("old log"));
        app.cmd_clean(true, false, false, Some(30), &mut Vec::new())
            .unwrap();
        assert!(!log.exists());
    }

    #[test]
    fn max_attempts_blocks_a_retry_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("demo")]);
        let mut state = State::default();
        let mut project = ProjectState::new("demo", "Demo", "/tmp/demo");
        project.status = ProjectStatus::Failed;
        project.attempts = app.cfg.max_attempts;
        state.upsert(project);
        app.save_state(&state).unwrap();
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                retry_failed: true,
                no_github: true,
                no_run: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("max attempts"), "{text}");
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].status, ProjectStatus::Failed);
    }

    #[test]
    fn stats_reset_requires_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        stats::save(&app.cfg.stats_file(), &stats::StatsStore::default()).unwrap();
        let err = app
            .cmd_stats(false, true, false, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("--yes"));
        assert!(app.cfg.stats_file().exists());
        app.cmd_stats(false, true, true, &mut Vec::new()).unwrap();
        assert!(!app.cfg.stats_file().exists());
    }

    #[test]
    fn check_requires_project_when_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let mut state = State::default();
        for slug in ["a", "b"] {
            state.upsert(ProjectState::new(slug, slug, "/tmp/x"));
        }
        app.save_state(&state).unwrap();
        let (_, result) = call_check(
            &mut app,
            Some(CheckAction::Add {
                title: "x".into(),
                project: None,
                due: None,
                kind: "manual".into(),
                priority: "normal".into(),
                detail: String::new(),
                every: None,
                probe: None,
            }),
            false,
            false,
            false,
            true,
        );
        assert!(result.unwrap_err().to_string().contains("several projects"));

        let (out, result) = call_check(
            &mut app,
            Some(CheckAction::Add {
                title: "x".into(),
                project: Some("a".into()),
                due: None,
                kind: "nope".into(),
                priority: "normal".into(),
                detail: String::new(),
                every: None,
                probe: None,
            }),
            false,
            false,
            false,
            true,
        );
        assert!(out.is_empty());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown check kind"));
    }

    #[test]
    fn status_shows_attention_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let mut store = checks::CheckStore::default();
        store.add(Check::new(
            "demo",
            "Verify cron",
            CheckKind::Vps,
            Priority::High,
            Some(Utc::now() + chrono::Duration::hours(1)),
            "",
            Utc::now(),
        ));
        checks::save(&app.cfg.checks_file(), &store).unwrap();
        let mut out = Vec::new();
        app.cmd_status(false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("checks: 1 now"), "{text}");
        assert!(text.contains("details: mox check"));
    }

    #[test]
    fn plan_notifies_about_open_questions() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        app.brain = Arc::new(ScriptedBrain::new(vec![
            r#"{"projects":[{"slug":"expense","title":"Expense","questions":[
                {"id":"q1","question":"Which DB?","default":"SQLite"}]}]}"#
                .to_string(),
        ]));
        let ideas = dir.path().join("ideas.txt");
        std::fs::write(&ideas, "- Expense\n").unwrap();
        let mut out = Vec::new();
        app.cmd_plan(
            PlanOptions {
                ideas: Some(ideas),
                yes: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let events = rec.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventKind::Questions);
        assert!(events[0].title.contains("1 question"));
        assert!(events[0].body.contains("expense"));
    }

    #[test]
    fn up_writes_and_pushes_recovery_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.cfg.recovery_enabled = true;
        app.cfg.recovery_interval_secs = 0;
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                no_github: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        assert!(app.cfg.state_dir_path().join("recovery/demo.md").exists());
        assert!(app
            .cfg
            .state_dir_path()
            .join("recovery/RECOVERY.md")
            .exists());
        let text =
            std::fs::read_to_string(app.cfg.state_dir_path().join("recovery/demo.md")).unwrap();
        assert!(text.contains("How to resume"), "{text}");
        assert!(text.contains("m1"), "{text}");
        let displays = app.runner_displays();
        assert!(displays.iter().any(|d| d.contains("RECOVERY.md")));
    }

    #[test]
    fn auto_reflection_is_written_under_mox() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.cfg.reflect_after_run = true;
        app.brain = Arc::new(ScriptedBrain::new(vec![
            "## What was achieved\nnothing".to_string()
        ]));
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                no_github: true,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let path = app.cfg.internal_dir().join("reflections/demo.md");
        assert!(path.exists(), "missing {}", path.display());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("What was achieved"));
        assert!(!app
            .cfg
            .projects_root_path()
            .join("demo/REFLECTION.md")
            .exists());
    }

    #[test]
    fn up_notifies_a_summary() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: true,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let events = rec.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventKind::Finished);
        assert!(events[0].title.contains("1 completed"));
        assert!(events[0].body.contains("demo: completed"));
    }

    #[test]
    fn up_notifies_failures_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "df" {
                return crate::exec::Output {
                    code: Some(0),
                    stdout: "x\n/dev/x 1 1 50000 1% /\n".into(),
                    ..crate::exec::Output::default()
                };
            }
            if spec.program == "gh" && spec.args.contains(&"create".to_string()) {
                return crate::exec::Output::failure(1, "Name already exists");
            }
            crate::exec::Output::ok()
        }));
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: false,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let events = rec.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventKind::Failed);
        assert!(events[0].title.contains("1 failed"));
    }

    #[test]
    fn check_sync_notifies_new_checks() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let project = app.cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("CHECKS.md"),
            "- [ ] Verify cron | due: +1h | kind: vps | priority: high\n",
        )
        .unwrap();
        let (_, result) = call_check(&mut app, None, false, false, false, false);
        result.unwrap();
        let events = rec.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventKind::Checks);
        assert!(events[0].body.contains("Verify cron"));
    }

    #[test]
    fn stats_command_reports_cross_project_calibration() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let mut store = stats::StatsStore::default();
        store.records.push(stats::MilestoneRecord {
            slug: "old".into(),
            milestone: "m1".into(),
            title: "old milestone".into(),
            complexity: crate::model::Complexity::High,
            estimate_secs: 100,
            actual_secs: 250,
            finished_at: Utc::now(),
            cpu_ghz: 3.5,
            logical_cores: 8,
        });
        stats::save(&app.cfg.stats_file(), &store).unwrap();
        let mut out = Vec::new();
        app.cmd_stats(false, false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed milestones: 1"), "{text}");
        assert!(text.contains("high"), "{text}");
        assert!(text.contains("2.50"), "{text}");

        let mut json_out = Vec::new();
        app.cmd_stats(true, false, false, &mut json_out).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(value["samples"], 1);
        assert_eq!(value["by_complexity"]["high"]["median_ratio"], 2.5);
    }

    #[test]
    fn notify_status_and_test_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let mut out = Vec::new();
        app.cmd_notify(NotifyAction::Status, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("none"));
        let err = app
            .cmd_notify(NotifyAction::Test, &mut Vec::new())
            .unwrap_err();
        assert!(err.to_string().contains("no notification channel"));

        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        app.ensure_layout().unwrap();
        let mut out = Vec::new();
        app.cmd_notify(NotifyAction::Test, &mut out).unwrap();
        assert_eq!(rec.events()[0].kind, EventKind::Test);
        assert!(String::from_utf8(out).unwrap().contains("recording"));
    }

    #[test]
    fn check_notify_sends_a_digest() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let mut store = checks::CheckStore::default();
        store.add(Check::new(
            "demo",
            "Check the VPS cron",
            CheckKind::Vps,
            Priority::High,
            Some(Utc::now() - chrono::Duration::minutes(5)),
            "",
            Utc::now(),
        ));
        checks::save(&app.cfg.checks_file(), &store).unwrap();
        let (out, result) = call_check(
            &mut app,
            Some(CheckAction::Notify),
            false,
            false,
            false,
            true,
        );
        result.unwrap();
        assert!(out.contains("reminder sent"), "{out}");
        let events = rec.events();
        assert_eq!(events[0].kind, EventKind::Digest);
        assert!(events[0].title.contains("overdue"));
        assert!(events[0].body.contains("Check the VPS cron"));
    }

    #[test]
    fn vps_approval_is_one_time_and_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);

        let mut out = Vec::new();
        app.cmd_vps_allow("nope", "", &mut out).unwrap_err();
        app.cmd_vps_allow("demo", "deploy the fetcher", &mut out)
            .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        let project = state.project("demo").unwrap();
        assert!(project.vps_approved_at.is_some());
        assert_eq!(project.vps_approved_note, "deploy the fetcher");

        let mut out = Vec::new();
        app.cmd_vps_approvals(&mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("demo"));

        let mut status = Vec::new();
        app.cmd_status(false, false, &mut status).unwrap();
        assert!(String::from_utf8(status).unwrap().contains("[vps-ok]"));

        let mut out = Vec::new();
        app.cmd_vps_revoke("demo", &mut out).unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert!(state.project("demo").unwrap().vps_approved_at.is_none());
    }

    #[test]
    fn up_warns_when_a_vps_project_is_not_approved() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        let mut spec = spec("demo");
        spec.needs_vps = true;
        write_plan(&app, vec![spec]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: None,
                no_github: true,
                no_run: true,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("needs VPS work but is not approved"),
            "{text}"
        );
        assert!(text.contains("mox vps allow demo"), "{text}");
    }

    #[test]
    fn up_imports_checks_written_in_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let project = app.cfg.projects_root_path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("CHECKS.md"),
            "- [ ] Verify VPS service | due: +2h | kind: vps | priority: high\n",
        )
        .unwrap();
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(
            UpOptions {
                only: vec![],
                max_parallel: Some(1),
                no_github: true,
                no_run: false,
                force: false,
                ..Default::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("attention: 1 new human check"), "{text}");
        let store = checks::load(&app.cfg.checks_file()).unwrap();
        assert_eq!(store.open().len(), 1);
        assert_eq!(store.checks[0].title, "Verify VPS service");
    }

    #[test]
    fn up_refuses_to_run_while_locked_but_checks_still_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        state_with_demo(&app);
        write_plan(&app, vec![spec("demo")]);

        let held = app.up_lock().unwrap();
        let mut out = Vec::new();
        let err = app
            .cmd_up(
                UpOptions {
                    only: vec![],
                    max_parallel: None,
                    no_github: true,
                    no_run: true,
                    force: false,
                    ..Default::default()
                },
                &mut out,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("another mox is running"), "{err}");

        // The attention queue uses its own lock and stays usable.
        let (out, result) = call_check(&mut app, None, false, false, false, false);
        assert!(result.is_ok());
        assert!(
            out.contains("nothing to check") || out.contains("CHECKS"),
            "{out}"
        );
        drop(held);
        app.cmd_up(
            UpOptions {
                no_github: true,
                no_run: true,
                ..Default::default()
            },
            &mut Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn stop_refuses_non_agent_pid() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        std::fs::write(runner::pid_path(&app.cfg, "demo"), "1\n").unwrap();
        let mut out = Vec::new();
        if !process_is_agent(1) {
            let err = app.cmd_stop("demo", &mut out).unwrap_err();
            assert!(err.to_string().contains("refusing to kill"));
        }
    }

    #[test]
    fn clean_dry_run_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let cache = app.cfg.projects_root_path().join("demo/node_modules");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("x"), b"12345").unwrap();
        let mut out = Vec::new();
        app.cmd_clean(false, false, false, None, &mut out).unwrap();
        assert!(cache.exists());
        assert!(String::from_utf8(out).unwrap().contains("dry run"));
    }

    #[test]
    fn clean_apply_removes_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.ensure_layout().unwrap();
        let cache = app.cfg.projects_root_path().join("demo/node_modules");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("x"), b"12345").unwrap();
        let mut out = Vec::new();
        app.cmd_clean(true, false, false, None, &mut out).unwrap();
        assert!(!cache.exists());
    }

    #[test]
    fn vps_suggest_prints_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.runner = Arc::new(FakeRunner::new(|spec| {
            if spec.program == "ssh" {
                return Output {
                    code: Some(0),
                    stdout: "CORES=4\nHOST=vps\nMEM_MB=3799\nDISK_MB=35000\nLOAD1=0.1\n".into(),
                    ..Output::default()
                };
            }
            if spec.program == "df" {
                return Output {
                    code: Some(0),
                    stdout: "x\n/dev/x 1 1 50000 1% /\n".into(),
                    ..Output::default()
                };
            }
            Output::ok()
        }));
        let mut out = Vec::new();
        app.cmd_vps_suggest("heavy", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("target:"));
        assert!(text.contains("c3-8"));
    }

    // Access the runner's recorded calls through the trait (FakeRunner only).
    impl App {
        fn runner_displays(&self) -> Vec<String> {
            self.runner
                .recorded_calls()
                .unwrap_or_default()
                .iter()
                .map(|c| c.display())
                .collect()
        }
    }
}
