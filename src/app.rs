//! CLI command implementations. Every external effect goes through
//! [`CommandRunner`] or the injected [`Brain`], so the whole workflow can run in
//! tests with no network, no GitHub and no OpenCode.

use crate::brain::Brain;
use crate::clean;
use crate::cli::{Command, VpsCommand};
use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec};
use crate::github;
use crate::ideas;
use crate::model::{ComputeNeed, Plan, ProjectSpec, ProjectState, ProjectStatus};
use crate::prompts;
use crate::resources::{dir_size_bytes, DfProbe, DiskGuard, DiskProbe, PruneKind};
use crate::runner::{self, ProjectRun};
use crate::scaffold::scaffold_project;
use crate::scheduler::Scheduler;
use crate::state::{self, State};
use crate::util::{display_path, human_mb, now_rfc3339, tail_lines, truncate_chars};
use crate::vps::{self, HostCapacity};
use anyhow::{bail, Context, Result};
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

pub struct App {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub runner: Arc<dyn CommandRunner>,
    pub brain: Arc<dyn Brain>,
    pub dry_run: bool,
    pub interactive: bool,
    pub asker: Box<dyn Asker>,
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
        }
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
            Command::Status { json } => self.cmd_status(json, out),
            Command::Logs { slug, lines } => self.cmd_logs(&slug, lines, out),
            Command::Reflect { slug, lines } => self.cmd_reflect(&slug, lines, out),
            Command::Clean { apply, all, force } => self.cmd_clean(apply, all, force, out),
            Command::Stop { slug } => self.cmd_stop(&slug, out),
            Command::Vps { action } => match action {
                VpsCommand::Check => self.cmd_vps_check(out),
                VpsCommand::Suggest { compute } => self.cmd_vps_suggest(&compute, out),
            },
            Command::Ideas { file } => self.cmd_ideas(&file, out),
        }
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
        for dir in [
            self.cfg.projects_root_path(),
            self.cfg.state_dir_path(),
            self.cfg.logs_dir(),
            self.cfg.run_dir(),
            self.cfg.cache_dir(),
            self.cfg.tmp_dir(),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
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
        let cached = state::load_plan(&self.cfg.plan_file())?;
        let source_name;

        let mut specs: Vec<ProjectSpec> = if let Some(path) = &ideas_path {
            let ideas = ideas::load_ideas(path)?;
            writeln!(
                out,
                "asking the planning model about {} project idea(s)...",
                ideas.len()
            )?;
            let prompt = prompts::questions_prompt(&self.cfg, &ideas);
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
            specs,
        };
        state::save_plan(&self.cfg.plan_file(), &plan)?;

        std::fs::write(
            self.cfg.questions_file(),
            prompts::render_questions_markdown(&plan.specs),
        )?;
        if !self.cfg.answers_file().exists() {
            std::fs::write(
                self.cfg.answers_file(),
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
                if outcome.success() {
                    project.status = ProjectStatus::Completed;
                    project.last_error = None;
                    writeln!(
                        out,
                        "  {}: done in {}s (log: {})",
                        outcome.slug,
                        outcome.duration_secs,
                        display_path(&outcome.log_path)
                    )?;
                } else {
                    project.status = ProjectStatus::Failed;
                    let reason = if outcome.output.timed_out {
                        format!("timed out after {}s", self.cfg.run_timeout_secs)
                    } else {
                        format!("{}: {}", outcome.output.summary(), outcome.output.tail(3))
                    };
                    project.last_error = Some(reason.clone());
                    writeln!(out, "  {}: failed ({reason})", outcome.slug)?;
                }
                state.upsert(project);
            }
            self.save_state(&state)?;
        }

        if self.dry_run {
            writeln!(
                out,
                "\ndry run: no files created, no commands executed, no state written"
            )?;
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
            writeln!(out, "{}", serde_json::to_string_pretty(&state)?)?;
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
        writeln!(
            out,
            "{:<28} {:<11} {:<8} {:<7} {:>9}  repo",
            "slug", "status", "compute", "cores", "size"
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
            writeln!(
                out,
                "{:<28} {:<11} {:<8} {:<7} {:>9}  {}{}",
                project.slug,
                format!("{}{}", project.status.as_str(), running),
                project.compute.as_str(),
                cores,
                size,
                truncate_chars(&project.repo_url, 44),
                if project.last_error.is_some() {
                    " !"
                } else {
                    ""
                }
            )?;
        }
        Ok(())
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
        let saved = state::load_plan(&app.cfg.plan_file()).unwrap().unwrap();
        assert_eq!(saved.specs.len(), 1);
        assert_eq!(saved.specs[0].compute, ComputeNeed::Medium);
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
    fn up_skips_github_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = temp_app(dir.path());
        write_plan(&app, vec![spec("demo")]);
        let mut out = Vec::new();
        app.cmd_up(vec![], Some(1), true, false, false, &mut out)
            .unwrap();
        let state = state::load(&app.cfg.state_file()).unwrap();
        assert_eq!(state.projects["demo"].repo_url, "");
        let displays = app.runner_displays();
        assert!(!displays.iter().any(|d| d.contains("repo create")));
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
