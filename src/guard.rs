//! Enforced guardrails for autonomous agents.
//!
//! Prompt rules are not a security boundary, so each project gets a `bin/`
//! directory containing wrapper scripts for dangerous programs (`git`, `gh`,
//! `ssh`, `kill`, ...). The wrappers are placed first on the agent's `PATH` and
//! re-execute `mox __guard <program>`, which applies the rules below and then
//! execs the real binary.
//!
//! This is defence in depth, not a kernel sandbox: an agent that deliberately
//! calls `/usr/bin/gh` can still bypass it. The goal is to stop accidents and
//! make the unsafe path explicit, and it is paired with prompt rules and (later)
//! OpenCode permission configs.

use crate::config::Config;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Programs for which a wrapper is generated when the real binary exists.
pub const GUARDED_PROGRAMS: &[&str] = &[
    "git",
    "gh",
    "ssh",
    "scp",
    "sftp",
    "kill",
    "pkill",
    "killall",
    "systemctl",
    "service",
    "crontab",
    "at",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

impl Decision {
    fn deny(message: impl Into<String>) -> Self {
        Decision::Deny(message.into())
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Where the wrappers live for one project.
pub fn bin_dir(cfg: &Config, slug: &str) -> PathBuf {
    cfg.internal_dir().join("guard").join(slug).join("bin")
}

/// Apply the rules for one invocation.
pub fn check(program: &str, args: &[String]) -> Decision {
    match program {
        "git" => check_git(args),
        "gh" => check_gh(args),
        "ssh" | "scp" | "sftp" => Decision::deny(format!(
            "`{program}` is blocked: agents must not reach the VPS directly. \
             All remote work goes through mox and needs a one-time human approval \
             (`mox vps allow <slug>`). Record what you need in CHECKS.md instead."
        )),
        "kill" | "pkill" | "killall" => Decision::deny(format!(
            "`{program}` is blocked: other agents and programs run in parallel. \
             Never signal a process you did not start."
        )),
        "systemctl" | "service" | "crontab" | "at" => Decision::deny(format!(
            "`{program}` is blocked: system services and schedules belong to the owner. \
             Add a check in CHECKS.md with the exact command to run."
        )),
        _ => Decision::Allow,
    }
}

fn check_git(args: &[String]) -> Decision {
    let (sub, rest) = match git_subcommand(args) {
        Some(pair) => pair,
        None => return Decision::Allow,
    };

    match sub.as_str() {
        "clone" => {
            return Decision::deny(
                "`git clone` is blocked: this project is the only repository an agent may touch.",
            )
        }
        "credential" => {
            return Decision::deny(
                "`git credential` is blocked: credentials are not agent-readable.",
            )
        }
        "filter-branch" | "filter-repo" => {
            return Decision::deny("history rewriting is blocked; use a new commit.")
        }
        "remote" => {
            let action = rest.first().map(String::as_str).unwrap_or("");
            if matches!(
                action,
                "add" | "remove" | "rm" | "rename" | "set-url" | "set-head"
            ) {
                return Decision::deny(
                    "changing git remotes is blocked: only the private `origin` may be used.",
                );
            }
        }
        "submodule" => {
            if rest.first().map(String::as_str) == Some("add") {
                return Decision::deny("adding submodules is blocked (external repository).");
            }
        }
        "config" => {
            if rest.iter().any(|a| a == "--global" || a == "--system") {
                return Decision::deny(
                    "changing global/system git config is blocked; use local config only.",
                );
            }
        }
        "push" => {
            for arg in &rest {
                let lower = arg.to_ascii_lowercase();
                if lower.starts_with("--force")
                    || lower == "-f"
                    || lower == "--mirror"
                    || lower == "--delete"
                    || lower == "-d"
                {
                    return Decision::deny(
                        "force/mirror/delete pushes are blocked. Push a normal branch or commit.",
                    );
                }
                if arg.starts_with('+') {
                    return Decision::deny("force refspecs (`+ref`) are blocked.");
                }
                if looks_like_remote_url(arg) {
                    return Decision::deny(
                        "pushing to an explicit URL is blocked: only `origin` may be used.",
                    );
                }
            }
        }
        "fetch" | "pull" => {
            for arg in &rest {
                if looks_like_remote_url(arg) {
                    return Decision::deny(
                        "fetching from an explicit URL is blocked; this project is the only repo.",
                    );
                }
            }
        }
        _ => {}
    }
    Decision::Allow
}

fn git_subcommand(args: &[String]) -> Option<(String, Vec<String>)> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(
            arg.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
        ) {
            index += 2;
            continue;
        }
        if arg.starts_with("--git-dir=")
            || arg.starts_with("--work-tree=")
            || arg.starts_with("--namespace=")
        {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((arg.clone(), args[index + 1..].to_vec()));
    }
    None
}

fn check_gh(args: &[String]) -> Decision {
    let Some(first) = args.first() else {
        return Decision::Allow;
    };
    if matches!(
        first.as_str(),
        "--version" | "-v" | "version" | "help" | "--help"
    ) {
        return Decision::Allow;
    }
    match first.as_str() {
        "repo" => {
            let action = args.get(1).map(String::as_str).unwrap_or("");
            if action == "view" {
                Decision::Allow
            } else {
                Decision::deny(
                    "`gh repo` is restricted to `view`: agents cannot create, delete, edit, \
                     archive, transfer, clone or list repositories.",
                )
            }
        }
        "pr" | "issue" => Decision::Allow,
        "release" => Decision::Allow,
        "api" => {
            Decision::deny("`gh api` is blocked: it can mutate repositories and read credentials.")
        }
        "auth" => Decision::deny("`gh auth` is blocked: tokens are not agent-readable."),
        "search" => {
            Decision::deny("`gh search` is blocked: agents must not enumerate other repositories.")
        }
        "secret" | "variable" | "workflow" | "codespace" | "ssh-key" | "gpg-key" | "config"
        | "extension" | "alias" | "gist" => {
            Decision::deny(format!("`gh {first}` is blocked by the mox guardrails."))
        }
        other => Decision::deny(format!(
            "`gh {other}` is not on the agent allowlist (repo view, pr, issue, release)."
        )),
    }
}

fn looks_like_remote_url(arg: &str) -> bool {
    arg.contains("://") || arg.starts_with("git@") || arg.starts_with("ssh:")
}

/// Generate the wrappers for one project and return the `bin` directory.
pub fn ensure(cfg: &Config, slug: &str) -> Result<PathBuf> {
    let dir = bin_dir(cfg, slug);
    crate::util::ensure_dir_private(&dir)?;
    let mox = std::env::current_exe()
        .context("resolving the mox executable path")?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_exe().unwrap_or_default());

    for program in GUARDED_PROGRAMS {
        let configured = match *program {
            "git" => Some(cfg.git_bin.as_str()),
            "gh" => Some(cfg.gh_bin.as_str()),
            _ => None,
        };
        let real = crate::util::resolve_executable(configured.unwrap_or(program));
        // Never generate a wrapper whose "real" target is itself on PATH or
        // missing; that would recurse or loop.
        if !real.is_absolute() || !real.is_file() {
            continue;
        }
        if real.starts_with(&dir) {
            continue;
        }
        let script = format!(
            "#!/bin/sh\n# generated by mox — do not edit\nMOX_REAL_BIN={real} exec {mox} __guard {program} \"$@\"\n",
            real = shell_single_quote(&real.display().to_string()),
            mox = shell_single_quote(&mox.display().to_string()),
            program = shell_single_quote(program),
        );
        let path = dir.join(program);
        std::fs::write(&path, script)
            .with_context(|| format!("writing guard wrapper {}", path.display()))?;
        crate::util::set_mode(&path, 0o700)?;
    }

    // A short README so a human inspecting .mox understands the directory.
    let readme = dir.parent().map(|p| p.join("README.txt"));
    if let Some(readme) = readme {
        let _ = crate::util::write_private(
            &readme,
            "Generated by mox. These wrappers go first on the agent PATH and enforce\n\
             repository/VPS/process guardrails. Regenerated on every `mox up`.\n",
        );
    }
    Ok(dir)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Run inside a wrapper: enforce the rule, then exec the real binary.
/// Never returns.
pub fn dispatch(program: &str, args: &[String]) -> ! {
    match check(program, args) {
        Decision::Allow => {
            let real = std::env::var("MOX_REAL_BIN").unwrap_or_default();
            if real.is_empty() {
                eprintln!("mox guard: MOX_REAL_BIN is not set; refusing to run `{program}`");
                std::process::exit(126);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(&real).args(args).exec();
                eprintln!("mox guard: could not execute {real}: {err}");
                std::process::exit(126);
            }
            #[cfg(not(unix))]
            {
                let status = std::process::Command::new(&real).args(args).status();
                match status {
                    Ok(status) => std::process::exit(status.code().unwrap_or(126)),
                    Err(err) => {
                        eprintln!("mox guard: could not execute {real}: {err}");
                        std::process::exit(126);
                    }
                }
            }
        }
        Decision::Deny(message) => {
            eprintln!("mox guard: {message}");
            std::process::exit(126);
        }
    }
}

/// True when `dir` is the guard directory of a project.
pub fn is_guard_dir(dir: &Path) -> bool {
    dir.components().any(|c| c.as_os_str() == "guard")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn blocks_ssh_and_process_signals() {
        for program in ["ssh", "scp", "sftp"] {
            let decision = check(program, &args(&["vps-ovh"]));
            assert!(!decision.is_allowed(), "{program} must be blocked");
            match decision {
                Decision::Deny(message) => assert!(message.contains("VPS")),
                _ => unreachable!(),
            }
        }
        for program in ["kill", "pkill", "killall"] {
            assert!(!check(program, &args(&["-9", "1234"])).is_allowed());
        }
        assert!(!check("systemctl", &args(&["restart", "x"])).is_allowed());
        assert!(!check("crontab", &args(&["-e"])).is_allowed());
    }

    #[test]
    fn git_allows_normal_work() {
        for argv in [
            vec!["status"],
            vec!["add", "-A"],
            vec!["commit", "-m", "feat: push --force in a message is fine"],
            vec!["push", "origin", "main"],
            vec!["push", "-u", "origin", "feature/x"],
            vec!["checkout", "-b", "feature/x"],
            vec!["fetch", "origin"],
            vec!["-C", "/tmp/proj", "status"],
        ] {
            assert!(
                check("git", &args(&argv)).is_allowed(),
                "should allow git {argv:?}"
            );
        }
    }

    #[test]
    fn git_blocks_remote_mutations() {
        for argv in [
            vec!["push", "--force", "origin", "main"],
            vec!["push", "-f"],
            vec!["push", "--force-with-lease"],
            vec!["push", "--mirror", "origin"],
            vec!["push", "--delete", "origin", "main"],
            vec!["push", "origin", "+main"],
            vec!["push", "git@github.com:someone/other.git", "main"],
            vec!["push", "https://example.com/x.git"],
            vec!["remote", "add", "evil", "git@github.com:x/y.git"],
            vec!["remote", "set-url", "origin", "git@github.com:x/y.git"],
            vec!["remote", "remove", "origin"],
            vec!["clone", "https://github.com/x/y.git"],
            vec!["config", "--global", "user.name", "x"],
            vec!["filter-branch", "--all"],
            vec!["submodule", "add", "https://github.com/x/y.git"],
            vec!["credential", "fill"],
        ] {
            assert!(
                !check("git", &args(&argv)).is_allowed(),
                "should block git {argv:?}"
            );
        }
    }

    #[test]
    fn global_option_prefix_is_parsed() {
        assert!(!check(
            "git",
            &args(&["-C", "/tmp/p", "push", "--force", "origin", "main"])
        )
        .is_allowed());
        assert!(check(
            "git",
            &args(&["-c", "user.name=x", "commit", "-m", "push --force"])
        )
        .is_allowed());
    }

    #[test]
    fn gh_allows_review_work_only() {
        for argv in [
            vec!["repo", "view", "me/proj"],
            vec!["pr", "create", "--title", "x"],
            vec!["pr", "list"],
            vec!["issue", "create"],
            vec!["release", "create", "v1"],
            vec!["--version"],
        ] {
            assert!(
                check("gh", &args(&argv)).is_allowed(),
                "should allow gh {argv:?}"
            );
        }
        for argv in [
            vec!["repo", "create", "x", "--public"],
            vec!["repo", "delete", "x"],
            vec!["repo", "edit", "x", "--visibility", "public"],
            vec!["repo", "list"],
            vec!["api", "user"],
            vec!["auth", "token"],
            vec!["search", "repos", "x"],
            vec!["secret", "set", "X"],
            vec!["workflow", "run"],
            vec!["frobnicate"],
        ] {
            assert!(
                !check("gh", &args(&argv)).is_allowed(),
                "should block gh {argv:?}"
            );
        }
    }

    #[test]
    fn unknown_programs_are_allowed() {
        assert_eq!(check("cargo", &args(&["test"])), Decision::Allow);
        assert_eq!(check("python3", &args(&["x.py"])), Decision::Allow);
    }

    #[test]
    fn generates_private_wrappers_with_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            projects_root: dir.path().join("projects").display().to_string(),
            state_dir: dir.path().join("state").display().to_string(),
            git_bin: "/bin/sh".into(), // exists and is absolute
            gh_bin: "/bin/sh".into(),
            ..Config::default()
        };
        let bin = ensure(&cfg, "demo").unwrap();
        assert!(bin.is_dir());
        assert!(bin.join("git").is_file());
        assert!(bin.join("gh").is_file());
        let script = std::fs::read_to_string(bin.join("git")).unwrap();
        assert!(script.contains("__guard"));
        assert!(script.contains("MOX_REAL_BIN='/bin/sh'"));
        assert!(script.contains("exec '"));
        assert!(!script.contains("MOX_REAL_BIN='git'"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(bin.join("git"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn quotes_paths_with_spaces() {
        assert_eq!(shell_single_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_single_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn guard_dir_detection() {
        assert!(is_guard_dir(Path::new("/x/.mox/guard/demo/bin")));
        assert!(!is_guard_dir(Path::new("/x/projects/demo")));
    }
}
