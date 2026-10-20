//! CLI command implementations. Every external effect goes through
//! [`CommandRunner`] or the injected [`Brain`], so the whole workflow can run in
//! tests with no network, no GitHub and no OpenCode.

use crate::brain::Brain;
use crate::checks::{self, Check, CheckKind, Priority};
use crate::clean;
use crate::cli::{CheckAction, Command, NotifyAction, VpsCommand};
use crate::config::Config;
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
use crate::util::{display_path, human_mb, now_rfc3339, tail_lines, truncate_chars};
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
            Command::Init { force } => self.cmd_init(force, out),
            Command::Doctor { skip_vps } => self.cmd_doctor(skip_vps, out),
            Command::Plan {
                ideas,
                answers,
                refresh,
                yes,
                interactive,
            } => self.cmd_plan(ideas, answers, refresh, yes, interactive, out),
            Command::Up {
                only,
                max_parallel,
                no_github,
                no_run,
                force,
            } => self.cmd_up(only, max_parallel, no_github, no_run, force, out),
            Command::Status {
                json,
                watch,
                interval,
            } => {
                if watch {
                    self.cmd_status_live(interval, out)
                } else {
                    self.cmd_status(json, out)
                }
            }
            Command::Logs { slug, lines } => self.cmd_logs(&slug, lines, out),
            Command::Reflect { slug, lines } => self.cmd_reflect(&slug, lines, out),
            Command::Clean { apply, all, force } => self.cmd_clean(apply, all, force, out),
            Command::Stop { slug } => self.cmd_stop(&slug, out),
            Command::Notify { action } => self.cmd_notify(action, out),
            Command::Stats { json } => self.cmd_stats(json, out),
            Command::Check {
                action,
                json,
                oneline,
                watch,
                interval,
                all,
                no_sync,
            } => self.cmd_check(
                action,
                CheckOptions {
                    json,
                    oneline,
                    watch,
                    interval,
                    all,
                    no_sync,
                },
                out,
            ),
            Command::Vps { action } => match action {
                VpsCommand::Check => self.cmd_vps_check(out),
                VpsCommand::Suggest { compute } => self.cmd_vps_suggest(&compute, out),
                VpsCommand::Allow { slug, note } => self.cmd_vps_allow(&slug, &note, out),
                VpsCommand::Revoke { slug } => self.cmd_vps_revoke(&slug, out),
                VpsCommand::Approvals => self.cmd_vps_approvals(out),
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

    fn cmd_init(&mut self, force: bool, out: &mut dyn Write) -> Result<()> {
        let mut cfg = self.cfg.clone();
        cfg.apply_env();
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

    fn cmd_plan(
        &mut self,
        ideas_path: Option<PathBuf>,
        answers_path: Option<PathBuf>,
        refresh: bool,
        yes: bool,
        interactive: bool,
        out: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_layout()?;
        let _lock = self.up_lock()?;
        let cached = state::load_plan(&self.cfg.plan_file())?;
        let source_name;
        let host_profile: Option<crate::host::HostProfile>;

        let mut specs: Vec<ProjectSpec> = if let Some(path) = &ideas_path {
            let ideas = ideas::load_ideas(path)?;
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
            prompts::merge_specs(&ideas, assessed)
        } else if !refresh {
            match cached {
                Some(plan) => {
                    source_name = plan.source;
                    host_profile = plan.host.clone();
                    plan.specs
                }
                None => {
                    bail!("no plan to reuse: pass `--ideas <file>` (or run `mox plan --ideas ...`)")
                }
            }
        } else {
            bail!("`--refresh` needs `--ideas <file>` so the model has something to think about");
        };

        // Explicit answers file wins; otherwise apply the default one if it
        // exists (it is written as a template after the first plan).
        let answers_file = answers_path.or_else(|| {
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

        let should_ask = interactive || (!yes && self.interactive);
        if should_ask {
            self.collect_answers(&mut specs, out)?;
        }

        let plan = Plan {
            created_at: now_rfc3339(),
            source: source_name,
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

        writeln!(out)?;
        if let Some(host) = &plan.host {
            writeln!(out, "estimates for: {}", host.describe())?;
        }
        writeln!(out, "plan for {} project(s):", plan.specs.len())?;
        writeln!(
            out,
            "  {:<28} {:<10} {:<8} title",
            "slug", "compute", "questions"
        )?;
        for spec in &plan.specs {
            let unanswered = spec.unanswered().len();
            writeln!(
                out,
                "  {:<28} {:<10} {:<8} {}",
                spec.slug,
                spec.compute.as_str(),
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

    fn cmd_up(
        &mut self,
        only: Vec<String>,
        max_parallel: Option<usize>,
        no_github: bool,
        no_run: bool,
        force: bool,
        out: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_layout()?;
        let _lock = self.up_lock()?;
        let mut state = self.load_state()?;
        let plan = self.load_plan()?;

        let specs: Vec<ProjectSpec> = if only.is_empty() {
            plan.specs.clone()
        } else {
            for slug in &only {
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
                .filter(|s| only.contains(&s.slug))
                .cloned()
                .collect()
        };
        if specs.is_empty() {
            bail!("the plan has no projects");
        }

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let scheduler = Scheduler::new(
            cores,
            self.cfg.reserve_cores,
            max_parallel.unwrap_or(self.cfg.max_parallel),
        );

        if !self.dry_run {
            let df = DfProbe::new(self.runner.as_ref());
            let free = df.free_mb(&self.cfg.projects_root_path())?;
            let guard = DiskGuard::new(self.cfg.min_free_disk_mb, self.cfg.project_disk_budget_mb);
            if !force {
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

            if !no_github {
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

        if no_run {
            writeln!(out, "\nno agents launched (--no-run)")?;
            self.print_summary(&state, out)?;
            return Ok(());
        }

        let batches = scheduler.batches(runnable.len());
        for (batch_index, batch) in batches.iter().enumerate() {
            writeln!(
                out,
                "\nbatch {}/{}: {} agent(s)",
                batch_index + 1,
                batches.len(),
                batch.len()
            )?;
            let mut runs = Vec::new();
            for (slot, project_index) in batch.iter().enumerate() {
                let spec = &runnable[*project_index];
                let cores_for_agent = scheduler.cores_for(slot);
                let dir = self.cfg.projects_root_path().join(&spec.slug);
                let prompt = prompts::execution_prompt(&self.cfg, spec);
                let run = ProjectRun::new(spec, dir, prompt, cores_for_agent.clone());

                let mut project = state
                    .project(&spec.slug)
                    .cloned()
                    .unwrap_or_else(|| ProjectState::new(&spec.slug, &spec.title, ""));
                project.cores = cores_for_agent.clone();
                project.status = ProjectStatus::Running;
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
            if !self.dry_run {
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
                continue;
            }

            let outcomes = runner::execute_batch(self.runner.as_ref(), &self.cfg, runs);
            for outcome in outcomes {
                let mut project = state
                    .project(&outcome.slug)
                    .cloned()
                    .unwrap_or_else(|| ProjectState::new(&outcome.slug, "", ""));
                project.exit_code = outcome.output.code;
                project.pid = None;
                project.duration_secs = outcome.duration_secs;
                // A run is only done when its work is committed and pushed.
                let verification = if outcome.success() {
                    let dir = self.cfg.projects_root_path().join(&outcome.slug);
                    Some(crate::verify::verify(self.runner.as_ref(), &self.cfg, &dir))
                } else {
                    None
                };
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
                state.upsert(project);
            }
            self.save_state(&state)?;
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

    fn cmd_status(&mut self, json: bool, out: &mut dyn Write) -> Result<()> {
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
            let running = runner::read_pid(&self.cfg, &project.slug)
                .map(|pid| format!(" (pid {pid})"))
                .unwrap_or_default();
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
                format!("{}{}", project.status.as_str(), running),
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
        Ok(())
    }

    fn cmd_status_live(&mut self, interval: u64, out: &mut dyn Write) -> Result<()> {
        let interval = interval.max(1);
        loop {
            write!(out, "\x1b[2J\x1b[H")?;
            self.cmd_status(false, out)?;
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
        out: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_layout()?;
        let _lock = if apply && !self.dry_run {
            Some(self.up_lock()?)
        } else {
            None
        };
        let report = clean::scan(&self.cfg);
        if report.project_entries.is_empty() && report.internal_entries.is_empty() {
            writeln!(out, "nothing to clean")?;
            return Ok(());
        }
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
        Ok(())
    }

    fn cmd_stop(&mut self, slug: &str, out: &mut dyn Write) -> Result<()> {
        let mut state = self.load_state()?;
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

    fn cmd_stats(&mut self, json: bool, out: &mut dyn Write) -> Result<()> {
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
                let check = Check::new(&slug, title.trim(), kind, priority, due_at, detail, now);
                let id = store.add(check);
                checks::save(&self.cfg.checks_file(), &store)?;
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
                    write!(out, "{}", checks::render(&store, now, opts.all))?;
                }
            }
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
        app.cmd_plan(None, None, false, true, false, &mut out)
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
        app.cmd_plan(Some(ideas), None, false, true, false, &mut out)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("expense"));
        assert!(text.contains("estimates for:"), "{text}");
        let saved = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        assert_eq!(saved.specs.len(), 1);
        assert_eq!(saved.specs[0].compute, ComputeNeed::Medium);
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
        app.cmd_plan(Some(ideas), None, false, false, true, &mut out)
            .unwrap();
        let saved = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        assert_eq!(saved.specs[0].answers["q1"], "PostgreSQL");
        // q2 had no scripted answer but a default, so the default was used.
        assert_eq!(saved.specs[0].answers["q2"], "CLI");
    }

    #[test]
    fn up_dry_run_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        app.dry_run = true;
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(vec![], None, false, false, false, &mut out)
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
        app.cmd_up(vec![], Some(1), false, false, false, &mut out)
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
        app.cmd_up(vec![], None, true, false, false, &mut out)
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
        app.cmd_up(vec![], None, true, false, false, &mut out)
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
        app.cmd_up(vec![], Some(1), true, false, false, &mut out)
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
            .cmd_up(vec!["nope".into()], None, true, true, false, &mut out)
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
        app.cmd_up(vec![], Some(1), false, false, false, &mut out)
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
        app.cmd_status(false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("slug"));
        assert!(text.contains("demo"));
        let mut json_out = Vec::new();
        app.cmd_status(true, &mut json_out).unwrap();
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
        app.cmd_status(false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("progress"), "{text}");
        assert!(text.contains("m2/3"), "{text}");
        assert!(text.contains("33%"), "{text}");

        let mut json_out = Vec::new();
        app.cmd_status(true, &mut json_out).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(value["milestone_progress"]["demo"]["current"], "m2");
        assert_eq!(value["milestone_progress"]["demo"]["percent"], 33);
        assert_eq!(value["milestone_progress"]["demo"]["done"], 1);
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

        app.cmd_up(vec![], None, true, false, false, &mut Vec::new())
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
        app.cmd_status(false, &mut out).unwrap();
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
        app.cmd_plan(Some(ideas), None, false, true, false, &mut out)
            .unwrap();
        let events = rec.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventKind::Questions);
        assert!(events[0].title.contains("1 question"));
        assert!(events[0].body.contains("expense"));
    }

    #[test]
    fn up_notifies_a_summary() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(crate::notify::RecordingNotifier::default());
        let mut app = temp_app(dir.path()).with_notifier(rec.clone());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(vec![], None, true, false, false, &mut out)
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
        app.cmd_up(vec![], None, false, false, false, &mut out)
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
        app.cmd_stats(false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed milestones: 1"), "{text}");
        assert!(text.contains("high"), "{text}");
        assert!(text.contains("2.50"), "{text}");

        let mut json_out = Vec::new();
        app.cmd_stats(true, &mut json_out).unwrap();
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
        app.cmd_status(false, &mut status).unwrap();
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
        app.cmd_up(vec![], None, true, true, false, &mut out)
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
        app.cmd_up(vec![], Some(1), true, false, false, &mut out)
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
            .cmd_up(vec![], None, true, true, false, &mut out)
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
        app.cmd_up(vec![], None, true, true, false, &mut Vec::new())
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
        app.cmd_clean(false, false, false, &mut out).unwrap();
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
        app.cmd_clean(true, false, false, &mut out).unwrap();
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
