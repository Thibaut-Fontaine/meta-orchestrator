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

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mox"))
            .args(args)
            .current_dir(dir)
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
            .expect("running mox")
    }

    fn run_ok_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.run_in(dir, args);
        assert!(
            output.status.success(),
            "mox {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
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
    for file in [
        "README.md",
        "AGENTS.md",
        "TASK.md",
        "CHECKS.md",
        ".gitignore",
    ] {
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

#[test]
fn check_queue_imports_agent_files_and_manages_checks() {
    let h = harness("PRIVATE");
    let ideas = h.root.join("ideas.txt");
    h.run_ok(&["init"]);
    h.run_ok(&["plan", "--ideas", ideas.to_str().unwrap(), "--yes"]);

    // Simulate an agent that recorded two human checks while working.
    let project = h.projects.join("demo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("CHECKS.md"),
        "- [ ] Verify the fetcher cron started | due: +2h | kind: vps | priority: high | detail: systemctl status\n\
         - [ ] Review the first data batch | due: +3d | kind: data\n",
    )
    .unwrap();

    let view = h.run_ok(&["check"]);
    assert!(view.contains("NOW (1)"), "{view}");
    assert!(view.contains("SOON (1)"), "{view}");
    assert!(view.contains("Verify the fetcher cron started"), "{view}");
    assert!(view.contains("mox check done <id>"), "{view}");

    let json = h.run_ok(&["check", "--json"]);
    let views: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(views.as_array().unwrap().len(), 2);
    assert!(views
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["bucket"] == "now" && v["kind"] == "vps"));

    let line = h.run_ok(&["check", "--oneline"]);
    assert!(line.starts_with("checks: "), "{line}");
    assert!(line.contains("1 now"), "{line}");

    // Add a critical check from the CLI, then postpone and complete it.
    let added = h.run_ok(&[
        "check",
        "add",
        "Check the API quota",
        "--project",
        "demo",
        "--due",
        "+30m",
        "--kind",
        "api",
        "--priority",
        "critical",
    ]);
    let id = added.split_whitespace().nth(1).unwrap().to_string();
    let json = h.run_ok(&["check", "--json"]);
    assert!(json.contains("Check the API quota"), "{json}");

    let postponed = h.run_ok(&["check", "later", &id, "+5d"]);
    assert!(postponed.contains("rescheduled"), "{postponed}");

    let done = h.run_ok(&["check", "done", &id, "--note", "quota is fine"]);
    assert!(done.contains("quota is fine"), "{done}");
    let json = h.run_ok(&["check", "--json"]);
    assert!(!json.contains("Check the API quota"), "{json}");
    let all = h.run_ok(&["check", "--all"]);
    assert!(all.contains("DONE"), "{all}");
    assert!(all.contains("Check the API quota"), "{all}");

    // The status table advertises the queue.
    let status = h.run_ok(&["status"]);
    assert!(status.contains("checks:"), "{status}");
    assert!(status.contains("mox check"), "{status}");
}

#[test]
fn guard_blocks_dangerous_agent_commands() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.sh");
    write_executable(&real, "#!/bin/sh\necho \"REAL $@\"\nexit 0\n");

    let run_guard = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mox"))
            .arg("__guard")
            .args(args)
            .env("MOX_REAL_BIN", &real)
            .output()
            .expect("running guard")
    };

    let allowed = run_guard(&["git", "status"]);
    assert!(allowed.status.success());
    assert!(String::from_utf8_lossy(&allowed.stdout).contains("REAL status"));

    for args in [
        vec!["git", "push", "--force", "origin", "main"],
        vec!["git", "remote", "add", "evil", "git@github.com:x/y.git"],
        vec!["gh", "repo", "create", "x", "--public"],
        vec!["gh", "api", "user"],
        vec!["ssh", "vps-ovh", "hostname"],
        vec!["kill", "-9", "1234"],
        vec!["systemctl", "restart", "nginx"],
    ] {
        let output = run_guard(&args);
        assert_eq!(
            output.status.code(),
            Some(126),
            "expected refusal for {args:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("mox guard"), "{stderr}");
    }
}

#[test]
fn telegram_notification_keeps_the_token_out_of_argv() {
    use meta_orchestrator::config::Config;

    let h = harness("PRIVATE");
    h.run_ok(&["init"]);

    let fake_bin = h.root.join("fakebin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let curl = fake_bin.join("curl");
    write_executable(
        &curl,
        r#"#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = "--config" ]; then
    cp "$2" "$MOX_TEST_CURL_CONF"
  fi
  printf '%s\n' "$1" >> "$MOX_TEST_CURL_ARGV"
  shift
done
echo '{"ok":true}'
"#,
    );
    let conf_copy = h.root.join("curl.conf");
    let argv_log = h.root.join("curl.argv");

    let cfg = Config {
        projects_root: h.projects.display().to_string(),
        state_dir: h.state.display().to_string(),
        notify_telegram: true,
        telegram_chat_id: "424242".into(),
        telegram_token_env: "MOX_TEST_TELEGRAM_TOKEN".into(),
        ..Config::default()
    };
    cfg.write(&h.config, true).unwrap();

    let old_path = std::env::var("PATH").unwrap_or_default();
    let output = Command::new(env!("CARGO_BIN_EXE_mox"))
        .args(["notify", "test"])
        .env("MOX_CONFIG", &h.config)
        .env("MOX_PROJECTS_ROOT", &h.projects)
        .env("MOX_STATE_DIR", &h.state)
        .env("MOX_TEST_TELEGRAM_TOKEN", "secret-token-123")
        .env("MOX_TEST_CURL_CONF", &conf_copy)
        .env("MOX_TEST_CURL_ARGV", &argv_log)
        .env("PATH", format!("{}:{old_path}", fake_bin.display()))
        .output()
        .expect("running notify test");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("telegram"));

    let conf = std::fs::read_to_string(&conf_copy).unwrap();
    assert!(conf.contains("botsecret-token-123"));
    assert!(conf.contains("chat_id=424242"));
    let argv = std::fs::read_to_string(&argv_log).unwrap();
    assert!(!argv.contains("secret-token-123"), "token leaked into argv");

    let status = h.run_ok(&["notify", "status"]);
    assert!(status.contains("telegram"), "{status}");
    assert!(
        !status.contains("secret-token-123"),
        "token leaked in status"
    );
}

#[test]
fn milestones_drive_progress_and_eta() {
    use meta_orchestrator::milestones;

    let h = harness("PRIVATE");
    let ideas = h.root.join("ideas.txt");
    h.run_ok(&["init"]);
    h.run_ok(&["plan", "--ideas", ideas.to_str().unwrap(), "--yes"]);
    h.run_ok(&["up", "--no-run"]);
    let project = h.projects.join("demo");

    let board_path = milestones::board_path(&project);
    assert!(board_path.exists(), "scaffold must create MILESTONES.json");
    let board = milestones::load(&board_path).unwrap();
    assert_eq!(board.milestones.len(), 3, "fallback plan expected");
    assert!(board
        .milestones
        .iter()
        .all(|m| m.status == milestones::MilestoneStatus::Pending));

    let listed = h.run_ok_in(&project, &["milestone", "list"]);
    assert!(listed.contains("m1"), "{listed}");
    assert!(listed.contains("pending"), "{listed}");
    assert!(
        listed.contains("algorithm") || listed.contains("estimate"),
        "{listed}"
    );

    let started = h.run_ok_in(&project, &["milestone", "start", "m1"]);
    assert!(started.contains("started m1"), "{started}");
    assert!(started.contains("current: m1"), "{started}");

    // Pretend the milestone took five minutes so it produces a usable stat.
    let mut board = milestones::load(&board_path).unwrap();
    board.milestones[0].started_at = Some(chrono::Utc::now() - chrono::Duration::minutes(5));
    milestones::save(&board_path, &board).unwrap();

    h.run_ok_in(&project, &["milestone", "todo", "m1", "1"]);
    let done = h.run_ok_in(
        &project,
        &["milestone", "done", "m1", "--note", "tests are green"],
    );
    assert!(done.contains("completed m1"), "{done}");
    assert!(done.contains("1/3 milestones"), "{done}");

    let board = milestones::load(&board_path).unwrap();
    assert_eq!(
        board.milestones[0].status,
        milestones::MilestoneStatus::Done
    );
    assert!(board.milestones[0].todos[0].done);
    assert!(board.milestones[0].actual_secs >= 300);
    assert!(board.events.iter().any(|e| e.kind == "done"));

    // The completed milestone feeds cross-project calibration.
    let stats = h.run_ok(&["stats", "--json"]);
    let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["samples"], 1, "{stats}");
    let stats_text = h.run_ok(&["stats"]);
    assert!(
        stats_text.contains("completed milestones: 1"),
        "{stats_text}"
    );

    let status = h.run_ok_in(&project, &["milestone", "status"]);
    assert!(status.contains("1/3 milestones"), "{status}");
    assert!(status.contains("current: m2"), "{status}");
    assert!(status.contains("eta:"), "{status}");

    let table = h.run_ok(&["status"]);
    assert!(table.contains("m2/3"), "{table}");
    assert!(table.contains("progress"), "{table}");

    let json = h.run_ok(&["status", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["milestone_progress"]["demo"]["done"], 1);
    assert_eq!(value["milestone_progress"]["demo"]["current"], "m2");

    // A blocked milestone removes the ETA and is visible in the status.
    let blocked = h.run_ok_in(
        &project,
        &[
            "milestone",
            "block",
            "m2",
            "--note",
            "waiting for the owner",
        ],
    );
    assert!(blocked.contains("blocked"), "{blocked}");
    let status = h.run_ok_in(&project, &["milestone", "status"]);
    assert!(status.contains("ETA is unavailable"), "{status}");
}
