//! Remote execution on the VPS (M4).
//!
//! The workstation stays the source of truth. A project is mirrored to the VPS
//! with `rsync`, the agent runs there under a wall-clock `timeout`, and its git
//! history is brought back as a bundle that is fetched into
//! `refs/remotes/mox-remote/*`. If the remote branch fast-forwards `main`, we
//! merge it; otherwise the project is marked failed with an explicit "merge
//! required" note, and nothing is silently overwritten.
//!
//! No credentials are copied: the VPS needs its own `opencode` installation,
//! and the private repository is never cloned there.

use crate::config::Config;
use crate::exec::{run_checked, CommandRunner, CommandSpec, Output};
use crate::model::ProjectSpec;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local,
    Vps,
    Auto,
}

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Target::Local),
            "vps" | "remote" => Ok(Target::Vps),
            "auto" => Ok(Target::Auto),
            other => Err(format!("unknown target `{other}` (use local, vps or auto)")),
        }
    }
}

/// Result of one remote run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOutcome {
    pub output: Output,
    pub log_path: PathBuf,
    /// True when a bundle was fetched into `refs/remotes/mox-remote/*`.
    pub fetched: bool,
    /// Set when the remote work could not be fast-forward merged.
    pub merge_note: Option<String>,
}

/// What the VPS needs for remote runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteCheck {
    pub reachable: bool,
    pub rsync: bool,
    pub git: bool,
    pub opencode: bool,
    pub disk_free_mb: u64,
    pub error: Option<String>,
}

impl RemoteCheck {
    pub fn ready(&self) -> bool {
        self.reachable && self.rsync && self.git && self.opencode
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.reachable {
            missing.push("ssh");
        }
        if !self.rsync {
            missing.push("rsync");
        }
        if !self.git {
            missing.push("git");
        }
        if !self.opencode {
            missing.push("opencode");
        }
        missing
    }
}

/// Remote directory for a project (`$HOME/mox-remote/<slug>`).
pub fn remote_dir(cfg: &Config, slug: &str) -> String {
    let root = cfg.remote_root.trim_end_matches('/');
    if root == "~" {
        format!("~/{slug}")
    } else {
        format!("{root}/{slug}")
    }
}

pub fn rsync_to_spec(cfg: &Config, local: &Path, alias: &str, slug: &str) -> CommandSpec {
    let dest = format!("{alias}:{}/", remote_dir(cfg, slug));
    CommandSpec::new(&cfg.rsync_bin)
        .args([
            "-az",
            "--timeout=120",
            "--delete",
            "--exclude",
            ".mox/",
            "--exclude",
            "target/",
            "--exclude",
            "node_modules/",
            "--exclude",
            ".venv/",
            "--exclude",
            "dist/",
            "--exclude",
            "build/",
            "--exclude",
            "out/",
            "--exclude",
            ".next/",
            "--exclude",
            ".cache/",
        ])
        .arg(format!("{}/", local.display()))
        .arg(dest)
        .timeout(1800)
}

/// Create the remote `.mox` directory (excluded from the project mirror).
pub fn mkdir_spec(cfg: &Config, alias: &str, slug: &str) -> CommandSpec {
    ssh_spec(
        cfg,
        alias,
        &format!("mkdir -p {}/.mox/bin", remote_dir(cfg, slug)),
        30,
    )
}

/// Ship the mox binary so remote agents can run `mox milestone`.
pub fn rsync_mox_spec(cfg: &Config, alias: &str, slug: &str, local_mox: &Path) -> CommandSpec {
    CommandSpec::new(&cfg.rsync_bin)
        .args(["-az", "--timeout=120"])
        .arg(local_mox.display().to_string())
        .arg(format!("{alias}:{}/.mox/bin/mox", remote_dir(cfg, slug)))
        .timeout(300)
}

/// Fetch the live milestone board back after a remote run.
pub fn rsync_board_spec(cfg: &Config, alias: &str, slug: &str, local_board: &Path) -> CommandSpec {
    CommandSpec::new(&cfg.rsync_bin)
        .args(["-az", "--timeout=60"])
        .arg(format!("{alias}:{}/MILESTONES.json", remote_dir(cfg, slug)))
        .arg(local_board.display().to_string())
        .timeout(120)
}

pub fn rsync_bundle_spec(
    cfg: &Config,
    alias: &str,
    slug: &str,
    local_bundle: &Path,
) -> CommandSpec {
    CommandSpec::new(&cfg.rsync_bin)
        .args(["-az", "--timeout=120"])
        .arg(format!(
            "{alias}:{}/.mox/return.bundle",
            remote_dir(cfg, slug)
        ))
        .arg(local_bundle.display().to_string())
        .timeout(600)
}

pub fn ssh_spec(cfg: &Config, alias: &str, command: &str, timeout_secs: u64) -> CommandSpec {
    CommandSpec::new(&cfg.ssh_bin)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=30",
        ])
        .arg(alias)
        .arg(command)
        .timeout(timeout_secs)
}

/// Preflight: tools + disk on the VPS (read-only).
pub fn preflight_spec(cfg: &Config, alias: &str) -> CommandSpec {
    let bin = &cfg.remote_opencode_bin;
    ssh_spec(
        cfg,
        alias,
        &format!(
            "printf 'RSYNC=%s\\n' \"$(command -v rsync || true)\"; \
             printf 'GIT=%s\\n' \"$(command -v git || true)\"; \
             printf 'OPENCODE=%s\\n' \"$(command -v {bin} || true)\"; \
             printf 'DISK_MB=%s\\n' \"$(df -Pm / | awk 'NR==2{{print $4}}')\""
        ),
        30,
    )
}

pub fn parse_preflight(stdout: &str) -> RemoteCheck {
    let mut check = RemoteCheck {
        reachable: true,
        ..RemoteCheck::default()
    };
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "RSYNC" => check.rsync = !value.is_empty(),
            "GIT" => check.git = !value.is_empty(),
            "OPENCODE" => check.opencode = !value.is_empty(),
            "DISK_MB" => check.disk_free_mb = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    check
}

/// Run the read-only preflight.
pub fn check(runner: &dyn CommandRunner, cfg: &Config, alias: &str) -> RemoteCheck {
    match runner.run(&preflight_spec(cfg, alias)) {
        Ok(output) if output.success() => {
            let mut check = parse_preflight(&output.stdout);
            if !check.reachable {
                check.error = Some("ssh preflight failed".to_string());
            }
            check
        }
        Ok(output) => RemoteCheck {
            error: Some(format!(
                "ssh preflight failed ({}): {}",
                output.summary(),
                output.tail(4)
            )),
            ..RemoteCheck::default()
        },
        Err(err) => RemoteCheck {
            error: Some(format!("could not run ssh: {err:#}")),
            ..RemoteCheck::default()
        },
    }
}

/// Command executed on the VPS: write the prompt (base64), then run OpenCode
/// with a remote wall-clock timeout and project-local caches.
pub fn remote_run_spec(
    cfg: &Config,
    alias: &str,
    slug: &str,
    prompt: &str,
    timeout_secs: u64,
) -> CommandSpec {
    let encoded = crate::util::base64_encode(prompt.as_bytes());
    let dir = remote_dir(cfg, slug);
    let bin = &cfg.remote_opencode_bin;
    let command = format!(
        "cd {dir} && mkdir -p .mox && printf %s '{encoded}' | base64 -d > .mox/prompt.txt && \
         PATH=\"$PWD/.mox/bin:$PATH\" TMPDIR=\"$PWD/.mox/tmp\" CARGO_TARGET_DIR=\"$PWD/.mox/cache/cargo-target\" \
         npm_config_cache=\"$PWD/.mox/cache/npm\" PIP_CACHE_DIR=\"$PWD/.mox/cache/pip\" \
         GIT_TERMINAL_PROMPT=0 GH_PROMPT_DISABLED=1 \
         GIT_AUTHOR_NAME=mox GIT_AUTHOR_EMAIL=mox@localhost \
         GIT_COMMITTER_NAME=mox GIT_COMMITTER_EMAIL=mox@localhost \
         nice -n 10 timeout {timeout_secs} {bin} run --auto --standalone \
         --title \"mox: {slug} (remote)\" \"$(cat .mox/prompt.txt)\""
    );
    ssh_spec(cfg, alias, &command, timeout_secs + 120)
}

pub fn bundle_spec(cfg: &Config, alias: &str, slug: &str) -> CommandSpec {
    let command = format!(
        "cd {} && mkdir -p .mox && git bundle create .mox/return.bundle --all",
        remote_dir(cfg, slug)
    );
    ssh_spec(cfg, alias, &command, 300)
}

pub fn fetch_spec(cfg: &Config, project_dir: &Path, bundle: &Path) -> CommandSpec {
    CommandSpec::new(&cfg.git_bin)
        .args([
            "fetch",
            &bundle.display().to_string(),
            "refs/heads/*:refs/remotes/mox-remote/*",
        ])
        .cwd(project_dir)
        .timeout(300)
}

pub fn merge_spec(cfg: &Config, project_dir: &Path) -> CommandSpec {
    CommandSpec::new(&cfg.git_bin)
        .args(["merge", "--ff-only", "mox-remote/main"])
        .cwd(project_dir)
        .timeout(120)
}

/// Mirror, run, and fetch back one project.
pub fn execute(
    runner: &dyn CommandRunner,
    cfg: &Config,
    spec: &ProjectSpec,
    project_dir: &Path,
    prompt: &str,
    alias: &str,
) -> Result<RemoteOutcome> {
    let slug = &spec.slug;
    run_checked(
        runner,
        &rsync_to_spec(cfg, project_dir, alias, slug),
        "rsync to VPS",
    )
    .context("copying the project to the VPS")?;

    // Remote milestone tracking needs the mox binary; it is copied, never
    // installed system-wide.
    if runner.run(&mkdir_spec(cfg, alias, slug))?.success() {
        if let Ok(exe) = std::env::current_exe() {
            let _ = runner.run(&rsync_mox_spec(cfg, alias, slug, &exe));
        }
    }

    crate::util::ensure_dir_private(&cfg.logs_dir())?;
    let log_path = cfg.logs_dir().join(format!("{slug}.remote.log"));
    let output = runner.run_logged(
        &remote_run_spec(cfg, alias, slug, prompt, cfg.run_timeout_secs),
        &log_path,
        &format!("remote run {slug}"),
    )?;

    // Bring the live board back even if the run failed halfway.
    let _ = runner.run(&rsync_board_spec(
        cfg,
        alias,
        slug,
        &crate::milestones::board_path(project_dir),
    ));

    let mut fetched = false;
    let mut merge_note = None;
    // Even a failed run may have committed useful work; try to bring it back.
    if !output.cancelled {
        crate::util::ensure_dir_private(&cfg.internal_dir())?;
        let bundle = cfg.internal_dir().join(format!("remote-{slug}.bundle"));
        if runner.run(&bundle_spec(cfg, alias, slug))?.success()
            && runner
                .run(&rsync_bundle_spec(cfg, alias, slug, &bundle))?
                .success()
        {
            let fetch = runner.run(&fetch_spec(cfg, project_dir, &bundle))?;
            if fetch.success() {
                fetched = true;
                let merge = runner.run(&merge_spec(cfg, project_dir))?;
                if !merge.success() {
                    merge_note = Some(format!(
                        "remote commits fetched to mox-remote/main but not merged: {}",
                        merge.tail(3)
                    ));
                }
            }
        }
    }

    Ok(RemoteOutcome {
        output,
        log_path,
        fetched,
        merge_note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FakeRunner;
    use crate::model::Idea;

    fn cfg() -> Config {
        Config {
            remote_root: "~/mox-remote".into(),
            run_timeout_secs: 600,
            ..Config::default()
        }
    }

    fn spec() -> ProjectSpec {
        ProjectSpec::from_idea(
            &Idea {
                title: Some("Demo".into()),
                details: "x".into(),
                raw: String::new(),
            },
            "demo".into(),
        )
    }

    #[test]
    fn remote_paths_and_rsync_excludes() {
        let cfg = cfg();
        assert_eq!(remote_dir(&cfg, "demo"), "~/mox-remote/demo");
        let spec = rsync_to_spec(&cfg, Path::new("/p/demo"), "vps-ovh", "demo");
        assert_eq!(spec.program, "rsync");
        assert!(spec.args.contains(&"--delete".to_string()));
        assert!(spec.args.contains(&"target/".to_string()));
        assert!(spec.args.iter().any(|a| a == "/p/demo/"));
        assert!(spec.args.iter().any(|a| a == "vps-ovh:~/mox-remote/demo/"));
    }

    #[test]
    fn prompt_is_base64_encoded_in_the_remote_command() {
        let cfg = cfg();
        let prompt = "line 1\nline '2' with \"quotes\" and $(danger)";
        let spec = remote_run_spec(&cfg, "vps-ovh", "demo", prompt, 600);
        let command = spec.args.last().unwrap();
        assert!(command.contains("base64 -d"));
        assert!(!command.contains("line '2'"));
        let encoded = crate::util::base64_encode(prompt.as_bytes());
        assert!(command.contains(&encoded));
        assert!(command.contains("timeout 600"));
        assert!(command.contains("--standalone"));
    }

    #[test]
    fn parses_preflight() {
        let check = parse_preflight(
            "RSYNC=/usr/bin/rsync\nGIT=/usr/bin/git\nOPENCODE=/home/u/.opencode/bin/opencode\nDISK_MB=32000\n",
        );
        assert!(check.ready());
        assert_eq!(check.disk_free_mb, 32000);
        let missing = parse_preflight("RSYNC=/usr/bin/rsync\nGIT=\nOPENCODE=\nDISK_MB=1\n");
        assert!(!missing.ready());
        assert_eq!(missing.missing(), vec!["git", "opencode"]);
    }

    #[test]
    fn check_reports_unreachable() {
        let runner = FakeRunner::new(|_| Output::failure(255, "ssh: connect refused"));
        let result = check(&runner, &cfg(), "vps-ovh");
        assert!(!result.reachable);
        assert!(result.error.unwrap().contains("ssh preflight failed"));
    }

    #[test]
    fn execute_mirrors_runs_and_fetches_back() {
        let runner = FakeRunner::new(|_| Output::ok());
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            projects_root: dir.path().join("projects").display().to_string(),
            state_dir: dir.path().join("state").display().to_string(),
            ..Config::default()
        };
        let outcome = execute(&runner, &cfg, &spec(), dir.path(), "do work", "vps-ovh").unwrap();
        assert!(outcome.output.success());
        assert!(outcome.fetched);
        assert!(outcome.merge_note.is_none());
        let displays = runner.displays();
        assert!(displays.iter().any(|d| d.starts_with("rsync -az")));
        assert!(displays.iter().any(|d| d.contains("ssh")));
        assert!(displays.iter().any(|d| d.contains("git fetch")));
        assert!(displays.iter().any(|d| d.contains("git merge --ff-only")));
        assert!(displays.iter().any(|d| d.contains(".mox/bin/mox")));
        let run = displays
            .iter()
            .find(|d| d.contains("opencode run"))
            .unwrap();
        assert!(run.contains(".mox/bin"));
    }

    #[test]
    fn failed_merge_is_reported_not_hidden() {
        let runner = FakeRunner::new(|spec| {
            if spec.program == "git" && spec.args.contains(&"merge".to_string()) {
                Output::failure(1, "not possible to fast-forward")
            } else {
                Output::ok()
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            projects_root: dir.path().join("projects").display().to_string(),
            state_dir: dir.path().join("state").display().to_string(),
            ..Config::default()
        };
        let outcome = execute(&runner, &cfg, &spec(), dir.path(), "x", "vps-ovh").unwrap();
        assert!(outcome.fetched);
        assert!(outcome
            .merge_note
            .as_deref()
            .unwrap()
            .contains("not merged"));
    }
}
