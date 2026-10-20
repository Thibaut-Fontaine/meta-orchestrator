//! Launching OpenCode agents: one isolated process per project, pinned to its
//! own cores, with caches redirected outside the repository.

use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec, Output};
use crate::model::ProjectSpec;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Everything needed to launch one project agent.
#[derive(Debug, Clone)]
pub struct ProjectRun {
    pub slug: String,
    pub dir: PathBuf,
    pub prompt: String,
    pub cores: Vec<usize>,
    pub compute: crate::model::ComputeNeed,
}

impl ProjectRun {
    pub fn new(spec: &ProjectSpec, dir: PathBuf, prompt: String, cores: Vec<usize>) -> Self {
        ProjectRun {
            slug: spec.slug.clone(),
            dir,
            prompt,
            cores,
            compute: spec.compute,
        }
    }
}

/// Outcome of one agent run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub slug: String,
    pub output: Output,
    pub log_path: PathBuf,
    pub duration_secs: u64,
    pub cores: Vec<usize>,
}

impl RunOutcome {
    pub fn success(&self) -> bool {
        self.output.success()
    }
}

pub fn log_path(cfg: &Config, slug: &str) -> PathBuf {
    cfg.logs_dir().join(format!("{slug}.log"))
}

pub fn prompt_path(cfg: &Config, slug: &str) -> PathBuf {
    cfg.logs_dir().join(format!("{slug}.prompt.txt"))
}

pub fn pid_path(cfg: &Config, slug: &str) -> PathBuf {
    cfg.run_dir().join(format!("{slug}.pid"))
}

/// Build the exact OpenCode command for one project.
///
/// Command shape:
/// `taskset -c <cores> nice -n <level> ionice -c 3 opencode run [--auto]
///  [--model M] [--standalone] --title "mox: <slug>" <prompt>`
pub fn build_run_command(cfg: &Config, run: &ProjectRun) -> CommandSpec {
    let mut spec = CommandSpec::new(&cfg.opencode_bin)
        .arg("run")
        .cwd(&run.dir)
        .timeout(cfg.run_timeout_secs)
        .env("MOX_PROJECT", &run.slug)
        .env(
            "MOX_PROJECTS_ROOT",
            cfg.projects_root_path().display().to_string(),
        )
        // Keep temp and build fallout inside the pruneable cache tree.
        .env("TMPDIR", cfg.tmp_for(&run.slug).display().to_string())
        .env(
            "CARGO_TARGET_DIR",
            cfg.cargo_target_for(&run.slug).display().to_string(),
        )
        .env(
            "npm_config_cache",
            cfg.cache_dir().join("npm").display().to_string(),
        )
        .env(
            "PIP_CACHE_DIR",
            cfg.cache_dir().join("pip").display().to_string(),
        )
        // Unattended: fail instead of waiting on a credential prompt.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("CARGO_TERM_COLOR", "never")
        // Guard wrappers first on PATH: git/gh/ssh/kill/systemctl rules.
        .env(
            "PATH",
            crate::util::path_with_prefix(&crate::guard::bin_dir(cfg, &run.slug)),
        )
        .pid_file(pid_path(cfg, &run.slug));

    if cfg.auto_approve {
        spec = spec.arg("--auto");
    }
    if !cfg.model.trim().is_empty() {
        spec = spec.arg("--model").arg(cfg.model.trim());
    }
    if cfg.standalone {
        spec = spec.arg("--standalone");
    }
    spec = spec
        .arg("--title")
        .arg(format!("mox: {}", run.slug))
        .arg(&run.prompt);

    if !cfg.ionice_bin.trim().is_empty() {
        spec = spec.wrap(cfg.ionice_bin.clone(), vec!["-c".into(), "3".into()]);
    }
    if !cfg.nice_bin.trim().is_empty() {
        spec = spec.wrap(
            cfg.nice_bin.clone(),
            vec!["-n".into(), cfg.nice_level.to_string()],
        );
    }
    if !cfg.taskset_bin.trim().is_empty() && !run.cores.is_empty() {
        let list = run
            .cores
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");
        spec = spec.wrap(cfg.taskset_bin.clone(), vec!["-c".into(), list]);
    }
    spec
}

/// Execute one run: persist the prompt, stream output to the project log, and
/// report timing.
pub fn execute_run(
    runner: &dyn CommandRunner,
    cfg: &Config,
    run: &ProjectRun,
) -> Result<RunOutcome> {
    std::fs::create_dir_all(cfg.logs_dir())?;
    std::fs::create_dir_all(cfg.run_dir())?;
    std::fs::create_dir_all(cfg.tmp_for(&run.slug))?;
    std::fs::create_dir_all(cfg.cargo_target_for(&run.slug))?;

    let prompt_file = prompt_path(cfg, &run.slug);
    std::fs::write(
        &prompt_file,
        format!("# prompt for {}\n\n{}", run.slug, run.prompt),
    )?;

    let spec = build_run_command(cfg, run);
    let log = log_path(cfg, &run.slug);
    let started = std::time::Instant::now();
    let output = runner.run_logged(&spec, &log, &format!("run {}", run.slug))?;
    Ok(RunOutcome {
        slug: run.slug.clone(),
        output,
        log_path: log,
        duration_secs: started.elapsed().as_secs(),
        cores: run.cores.clone(),
    })
}

/// Run a batch concurrently (one thread per project) and return outcomes in the
/// same order as `runs`. A failing run never aborts its siblings.
pub fn execute_batch(
    runner: &dyn CommandRunner,
    cfg: &Config,
    runs: Vec<ProjectRun>,
) -> Vec<RunOutcome> {
    let outcomes: Vec<RunOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = runs
            .iter()
            .map(|run| {
                let slug = run.slug.clone();
                let handle = scope.spawn(move || match execute_run(runner, cfg, run) {
                    Ok(outcome) => outcome,
                    Err(err) => RunOutcome {
                        slug,
                        output: Output::failure(-1, format!("failed to launch: {err:#}")),
                        log_path: PathBuf::new(),
                        duration_secs: 0,
                        cores: Vec::new(),
                    },
                });
                (run.slug.clone(), handle)
            })
            .collect();

        handles
            .into_iter()
            .map(|(slug, handle)| {
                handle.join().unwrap_or_else(|_| RunOutcome {
                    slug,
                    output: Output::failure(-1, "agent thread panicked"),
                    log_path: PathBuf::new(),
                    duration_secs: 0,
                    cores: Vec::new(),
                })
            })
            .collect()
    });
    outcomes
}

/// Execute a rolling pool: `max_parallel` workers pull projects from a shared
/// queue, so a finishing agent immediately frees a slot for the next project
/// (no wave barrier). Each worker keeps its own core set.
pub fn execute_pool(
    runner: &dyn CommandRunner,
    cfg: &Config,
    runs: Vec<ProjectRun>,
    max_parallel: usize,
    slots: &[Vec<usize>],
) -> Vec<RunOutcome> {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    if runs.is_empty() {
        return Vec::new();
    }
    let workers = max_parallel.max(1).min(runs.len());
    let queue: Mutex<VecDeque<(usize, ProjectRun)>> =
        Mutex::new(runs.into_iter().enumerate().collect());
    let results: Mutex<Vec<(usize, RunOutcome)>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for worker in 0..workers {
            let cores = slots
                .get(worker % slots.len().max(1))
                .cloned()
                .unwrap_or_default();
            let queue = &queue;
            let results = &results;
            handles.push(scope.spawn(move || loop {
                if crate::shutdown::requested() {
                    break;
                }
                let job = queue.lock().unwrap().pop_front();
                let Some((index, mut run)) = job else {
                    break;
                };
                run.cores = cores.clone();
                let outcome = match execute_run(runner, cfg, &run) {
                    Ok(outcome) => outcome,
                    Err(err) => RunOutcome {
                        slug: run.slug.clone(),
                        output: Output::failure(-1, format!("failed to launch: {err:#}")),
                        log_path: PathBuf::new(),
                        duration_secs: 0,
                        cores: run.cores.clone(),
                    },
                };
                results.lock().unwrap().push((index, outcome));
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
    });

    let mut collected = results.into_inner().unwrap_or_default();
    collected.sort_by_key(|(index, _)| *index);
    collected.into_iter().map(|(_, outcome)| outcome).collect()
}

/// Remove a stale pid file (used after a completed/stopped run).
pub fn clear_pid_file(cfg: &Config, slug: &str) {
    let _ = std::fs::remove_file(pid_path(cfg, slug));
}

/// Read a live pid for a project, if any.
pub fn read_pid(cfg: &Config, slug: &str) -> Option<u32> {
    std::fs::read_to_string(pid_path(cfg, slug))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

pub fn dir_display(path: &Path) -> String {
    crate::util::display_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FakeRunner;
    use crate::model::{ComputeNeed, Idea, ProjectSpec};
    use std::time::Instant;

    fn cfg(root: &Path) -> Config {
        Config {
            projects_root: root.to_string_lossy().to_string(),
            state_dir: root.join(".state").to_string_lossy().to_string(),
            nice_level: 10,
            model: "test/model".into(),
            ..Config::default()
        }
    }

    fn spec(slug: &str) -> ProjectSpec {
        ProjectSpec::from_idea(
            &Idea {
                title: Some(slug.into()),
                details: "do things".into(),
                raw: String::new(),
            },
            slug.into(),
        )
    }

    #[test]
    fn command_has_pinning_priority_and_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let run = ProjectRun::new(
            &spec("demo"),
            dir.path().join("demo"),
            "the prompt".into(),
            vec![1, 2, 3],
        );
        let cmd = build_run_command(&cfg, &run);
        assert_eq!(cmd.program, "taskset");
        assert_eq!(
            cmd.args,
            vec![
                "-c",
                "1,2,3",
                "nice",
                "-n",
                "10",
                "ionice",
                "-c",
                "3",
                "opencode",
                "run",
                "--auto",
                "--model",
                "test/model",
                "--standalone",
                "--title",
                "mox: demo",
                "the prompt"
            ]
        );
        assert_eq!(cmd.env.get("MOX_PROJECT").unwrap(), "demo");
        assert!(cmd.env.get("TMPDIR").unwrap().contains("tmp/demo"));
        assert!(cmd
            .env
            .get("CARGO_TARGET_DIR")
            .unwrap()
            .contains("cache/cargo-target/demo"));
        assert_eq!(cmd.env.get("GIT_TERMINAL_PROMPT").unwrap(), "0");
        assert!(cmd.env.get("PATH").unwrap().contains("guard/demo/bin"));
        assert_eq!(cmd.env.get("GH_PROMPT_DISABLED").unwrap(), "1");
        assert!(cmd.pid_file.is_some());
        assert_eq!(cmd.timeout_secs, cfg.run_timeout_secs);
    }

    #[test]
    fn wrappers_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            taskset_bin: String::new(),
            nice_bin: String::new(),
            ionice_bin: String::new(),
            auto_approve: false,
            standalone: true,
            model: String::new(),
            ..cfg(dir.path())
        };
        let run = ProjectRun::new(&spec("demo"), dir.path().into(), "p".into(), vec![0]);
        let cmd = build_run_command(&cfg, &run);
        assert_eq!(cmd.program, "opencode");
        assert_eq!(
            cmd.args,
            vec!["run", "--standalone", "--title", "mox: demo", "p"]
        );
    }

    #[test]
    fn execute_run_writes_prompt_and_log() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let run = ProjectRun::new(
            &spec("demo"),
            dir.path().into(),
            "hello agent".into(),
            vec![0],
        );
        let runner = FakeRunner::ok();
        let outcome = execute_run(&runner, &cfg, &run).unwrap();
        assert!(outcome.success());
        let prompt = std::fs::read_to_string(prompt_path(&cfg, "demo")).unwrap();
        assert!(prompt.contains("hello agent"));
        let log = std::fs::read_to_string(log_path(&cfg, "demo")).unwrap();
        assert!(log.contains("run demo"));
        assert!(cfg.tmp_for("demo").is_dir());
        assert!(cfg.cargo_target_for("demo").is_dir());
    }

    #[test]
    fn batch_runs_in_parallel_and_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            max_parallel: 2,
            ..cfg(dir.path())
        };
        let runner = FakeRunner::new(|_| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            Output::ok()
        });
        let runs = vec![
            ProjectRun::new(&spec("a"), dir.path().into(), "a".into(), vec![0]),
            ProjectRun::new(&spec("b"), dir.path().into(), "b".into(), vec![1]),
        ];
        let started = Instant::now();
        let outcomes = execute_batch(&runner, &cfg, runs);
        let elapsed = started.elapsed();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].slug, "a");
        assert_eq!(outcomes[1].slug, "b");
        assert!(
            elapsed < std::time::Duration::from_millis(280),
            "batch took {elapsed:?}, expected parallel execution"
        );
    }

    #[test]
    fn pool_starts_next_project_without_a_wave_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let runner = FakeRunner::new(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            Output::ok()
        });
        let runs: Vec<ProjectRun> = ["a", "b", "c"]
            .iter()
            .map(|slug| {
                ProjectRun::new(
                    &spec(slug),
                    dir.path().to_path_buf(),
                    slug.to_string(),
                    vec![0],
                )
            })
            .collect();
        let started = Instant::now();
        let outcomes = execute_pool(&runner, &cfg, runs, 2, &[vec![0, 1], vec![2, 3]]);
        let elapsed = started.elapsed();
        assert_eq!(outcomes.len(), 3);
        // 3 jobs x 100ms over 2 workers: ~200ms, not 300ms (two waves).
        assert!(
            elapsed < std::time::Duration::from_millis(280),
            "pool took {elapsed:?}"
        );
        let slugs: Vec<&str> = outcomes.iter().map(|o| o.slug.as_str()).collect();
        assert_eq!(slugs, vec!["a", "b", "c"]);
        assert!(
            outcomes
                .iter()
                .all(|o| o.cores == vec![0, 1] || o.cores == vec![2, 3]),
            "{outcomes:?}"
        );
    }

    #[test]
    fn one_failure_does_not_abort_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let runner = FakeRunner::new(|spec| {
            if spec.args.iter().any(|a| a == "fail") {
                Output::failure(7, "boom")
            } else {
                Output::ok()
            }
        });
        let runs = vec![
            ProjectRun::new(&spec("ok"), dir.path().into(), "fine".into(), vec![0]),
            ProjectRun::new(&spec("bad"), dir.path().into(), "fail".into(), vec![1]),
        ];
        let outcomes = execute_batch(&runner, &cfg, runs);
        assert!(outcomes[0].success());
        assert!(!outcomes[1].success());
        assert_eq!(outcomes[1].output.code, Some(7));
    }

    #[test]
    fn pid_file_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        std::fs::create_dir_all(cfg.run_dir()).unwrap();
        std::fs::write(pid_path(&cfg, "x"), "1234\n").unwrap();
        assert_eq!(read_pid(&cfg, "x"), Some(1234));
        clear_pid_file(&cfg, "x");
        assert_eq!(read_pid(&cfg, "x"), None);
    }

    #[test]
    fn compute_need_is_carried() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec("heavy");
        spec.compute = ComputeNeed::Heavy;
        let run = ProjectRun::new(&spec, dir.path().into(), "p".into(), vec![0, 1, 2, 3]);
        assert_eq!(run.compute, ComputeNeed::Heavy);
    }
}
