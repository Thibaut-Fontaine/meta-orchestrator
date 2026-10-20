//! End-to-end tests of the `mox` binary.
//!
//! These tests use the real `git` and `df`, but a fake `gh` and a fake
//! `opencode` on PATH, so they exercise the full workflow (scaffolding, private
//! repo creation, parallel launch, state, logs, cleanup) with no network, no
//! GitHub mutation and no model calls.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    projects: PathBuf,
    state: PathBuf,
    config: PathBuf,
    fake_opencode: PathBuf,
    fake_gh: PathBuf,
    gh_log: PathBuf,
}

fn write_executable(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn harness(visibility: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    let fake_opencode = bin.join("opencode");
    write_executable(
        &fake_opencode,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "fake-opencode 0.0.1"
  exit 0
fi
cat <<'JSON'
{"projects":[
  {"slug":"demo","title":"Demo project","summary":"A demo built by the fake brain.",
   "questions":[{"id":"q1","question":"Which storage?","default":"SQLite"}],
   "assumptions":["single user"],"risks":["none"],
   "stack":["Rust"],"compute":"light","needs_vps":false,
   "estimated_disk_mb":128,
   "acceptance_criteria":["builds","tests pass"]}
]}
JSON
"#,
    );

    let fake_gh = bin.join("gh");
    let gh_log = root.join("gh.log");
    write_executable(
        &fake_gh,
        &format!(
            r#"#!/bin/sh
echo "$@" >> "{log}"
if [ "$1" = "auth" ]; then exit 0; fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  echo '{{"visibility":"{visibility}","url":"https://github.com/test/demo"}}'
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "create" ]; then
  git remote add origin "git@github.com:test/$3.git" 2>/dev/null || true
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
            visibility = visibility
        ),
    );

    let ideas = root.join("ideas.txt");
    std::fs::write(&ideas, "- Demo project :: a small demo\n").unwrap();

    Harness {
        root: root.clone(),
        projects: root.join("projects"),
        state: root.join("state"),
        config: root.join("config.toml"),
        fake_opencode,
        fake_gh,
        gh_log,
        _dir: dir,
    }
}

impl Harness {
    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_mox"))
            .args(args)
            .env("MOX_CONFIG", &self.config)
            .env("MOX_PROJECTS_ROOT", &self.projects)
            .env("MOX_STATE_DIR", &self.state)
            .env("MOX_OPENCODE_BIN", &self.fake_opencode)
            .env("MOX_GH_BIN", &self.fake_gh)
            .env("MOX_GIT_BIN", "git")
            .env("MOX_MAX_PARALLEL", "1")
            .env("GIT_AUTHOR_NAME", "mox test")
            .env("GIT_AUTHOR_EMAIL", "mox@example.invalid")
            .env("GIT_COMMITTER_NAME", "mox test")
            .env("GIT_COMMITTER_EMAIL", "mox@example.invalid")
            .output()
            .expect("running mox");
        output
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "mox {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn state_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.state.join("state.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

#[test]
fn full_workflow_init_plan_up_status_clean() {
    let h = harness("PRIVATE");
    let ideas = h.root.join("ideas.txt");

    let out = h.run_ok(&["init"]);
    assert!(out.contains("projects root"));
    assert!(h.config.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&h.config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let out = h.run_ok(&["doctor", "--skip-vps"]);
    assert!(out.contains("all checks passed"), "{out}");

    let out = h.run_ok(&["plan", "--ideas", ideas.to_str().unwrap(), "--yes"]);
    assert!(out.contains("demo"), "{out}");
    assert!(h.state.join("plan.json").exists());
    assert!(h.state.join("answers.toml").exists());
    assert!(h.state.join("QUESTIONS.md").exists());

    let out = h.run_ok(&["up"]);
    assert!(
        out.contains("private repo: https://github.com/test/demo"),
        "{out}"
    );
    assert!(out.contains("done in"), "{out}");

    // The project is a real git repository with the scaffold committed.
    let project = h.projects.join("demo");
    for file in ["README.md", "AGENTS.md", "TASK.md", ".gitignore"] {
        assert!(project.join(file).exists(), "missing {file}");
    }
    let tracked = Command::new("git")
        .args(["ls-files"])
        .current_dir(&project)
        .output()
        .unwrap();
    let tracked = String::from_utf8_lossy(&tracked.stdout);
    assert!(tracked.contains("TASK.md"));
    assert!(tracked.contains(".gitignore"));

    // gh was told to create a private repository, never a public one.
    let gh_calls = std::fs::read_to_string(&h.gh_log).unwrap();
    assert!(
        gh_calls.contains("repo create demo --private"),
        "{gh_calls}"
    );
    assert!(!gh_calls.contains("--public"));

    // The agent log exists and captured the fake model output.
    let log = std::fs::read_to_string(h.state.join("..").join("projects/.mox/logs/demo.log"))
        .or_else(|_| std::fs::read_to_string(h.projects.join(".mox/logs/demo.log")))
        .unwrap();
    assert!(log.contains("opencode run"), "{log}");
    assert!(
        log.contains("Demo project") || log.contains("projects"),
        "{log}"
    );

    // State says completed, with a private URL and assigned cores.
    let state = h.state_json();
    let demo = &state["projects"]["demo"];
    assert_eq!(demo["status"], "completed");
    assert_eq!(demo["repo_url"], "https://github.com/test/demo");
    assert!(!demo["cores"].as_array().unwrap().is_empty());

    // Status renders a table and valid JSON.
    let table = h.run_ok(&["status"]);
    assert!(table.contains("demo"));
    let json = h.run_ok(&["status", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["projects"]["demo"]["title"], "Demo project");

    // Logs command returns the tail.
    let logs = h.run_ok(&["logs", "demo", "-n", "5"]);
    assert!(!logs.trim().is_empty());

    // Clean removes caches but keeps sources.
    std::fs::create_dir_all(project.join("node_modules/pkg")).unwrap();
    std::fs::write(project.join("node_modules/pkg/x.js"), b"hello").unwrap();
    let dry = h.run_ok(&["clean"]);
    assert!(dry.contains("dry run"));
    assert!(project.join("node_modules").exists());
    h.run_ok(&["clean", "--apply"]);
    assert!(!project.join("node_modules").exists());
    assert!(project.join("TASK.md").exists());
}

#[test]
fn public_repo_is_refused_and_agent_does_not_run() {
    let h = harness("PUBLIC");
    let ideas = h.root.join("ideas.txt");
    h.run_ok(&["init"]);
    h.run_ok(&["plan", "--ideas", ideas.to_str().unwrap(), "--yes"]);

    let out = h.run(&["up", "--max-parallel", "1"]);
    // The failure is reported per project and mox keeps going.
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("not private"), "{text}");

    let state = h.state_json();
    assert_eq!(state["projects"]["demo"]["status"], "failed");
    let log = h.projects.join(".mox/logs/demo.log");
    assert!(!log.exists(), "agent must not run for a non-private repo");
}

#[test]
fn dry_run_creates_nothing() {
    let h = harness("PRIVATE");
    let ideas = h.root.join("ideas.txt");
    h.run_ok(&["init"]);
    h.run_ok(&["plan", "--ideas", ideas.to_str().unwrap(), "--yes"]);

    let out = h.run_ok(&["--dry-run", "up"]);
    assert!(out.contains("[dry-run]"), "{out}");
    assert!(!h.projects.join("demo").exists());
    assert!(!h.projects.join(".mox/logs/demo.log").exists());
}

#[test]
fn plan_consumes_answers_file() {
    let h = harness("PRIVATE");
    let ideas = h.root.join("ideas.txt");
    h.run_ok(&["init"]);
    h.run_ok(&["plan", "--ideas", ideas.to_str().unwrap(), "--yes"]);

    let answers = h.root.join("answers.toml");
    std::fs::write(&answers, "[projects.demo]\nq1 = \"PostgreSQL\"\n").unwrap();
    let out = h.run_ok(&["plan", "--answers", answers.to_str().unwrap(), "--yes"]);
    assert!(out.contains("applied answers"), "{out}");

    let plan = std::fs::read_to_string(h.state.join("plan.json")).unwrap();
    assert!(plan.contains("PostgreSQL"), "{plan}");
}
