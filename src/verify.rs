//! Post-run verification: an agent run only counts as completed when its work
//! is committed and pushed.
//!
//! We deliberately avoid network calls. `git status` and the local
//! `origin/main` remote-tracking ref are enough to detect the common failure
//! modes (leftover edits, commits made after the last push). If no upstream ref
//! exists we report it as unverified rather than inventing a failure.

use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Verification {
    pub dirty_files: usize,
    pub unpushed_commits: usize,
    /// Set when the repository has no origin/main ref to compare against.
    pub unverified: Option<String>,
}

impl Verification {
    pub fn is_clean(&self) -> bool {
        self.dirty_files == 0 && self.unpushed_commits == 0
    }

    pub fn reason(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.dirty_files > 0 {
            parts.push(format!("{} uncommitted file(s)", self.dirty_files));
        }
        if self.unpushed_commits > 0 {
            parts.push(format!("{} unpushed commit(s)", self.unpushed_commits));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("post-run check failed: {}", parts.join(", ")))
        }
    }

    pub fn summary(&self) -> String {
        match self.reason() {
            Some(reason) => reason,
            None => match &self.unverified {
                Some(note) => format!("clean, but {note}"),
                None => "clean and pushed".to_string(),
            },
        }
    }
}

/// Inspect one project directory after an agent finished.
pub fn verify(runner: &dyn CommandRunner, cfg: &Config, dir: &Path) -> Verification {
    let git = |args: Vec<&str>| {
        CommandSpec::new(&cfg.git_bin)
            .args(args)
            .cwd(dir)
            .timeout(60)
    };

    let status = runner.run(&git(vec!["status", "--porcelain"]));
    let dirty_files = match status {
        Ok(output) if output.success() => output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        Ok(output) => {
            return Verification {
                unverified: Some(format!("git status failed ({})", output.summary())),
                ..Verification::default()
            }
        }
        Err(err) => {
            return Verification {
                unverified: Some(format!("git status failed: {err:#}")),
                ..Verification::default()
            }
        }
    };

    // Prefer the configured upstream, then origin/main.
    let mut base: Option<String> = None;
    if let Ok(output) = runner.run(&git(vec![
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{u}",
    ])) {
        if output.success() && !output.stdout.trim().is_empty() {
            base = Some(output.stdout.trim().to_string());
        }
    }
    if base.is_none() {
        if let Ok(output) = runner.run(&git(vec!["rev-parse", "--verify", "origin/main"])) {
            if output.success() && !output.stdout.trim().is_empty() {
                base = Some("origin/main".to_string());
            }
        }
    }

    let Some(base) = base else {
        return Verification {
            dirty_files,
            unpushed_commits: 0,
            unverified: Some("no upstream/origin-main ref to compare against".to_string()),
        };
    };

    let range = format!("{base}..HEAD");
    let count = runner.run(&git(vec!["rev-list", "--count", &range]));
    let unpushed_commits = match count {
        Ok(output) if output.success() => output.stdout.trim().parse().unwrap_or(0),
        Ok(output) => {
            return Verification {
                dirty_files,
                unpushed_commits: 0,
                unverified: Some(format!("rev-list failed ({})", output.summary())),
            }
        }
        Err(err) => {
            return Verification {
                dirty_files,
                unpushed_commits: 0,
                unverified: Some(format!("rev-list failed: {err:#}")),
            }
        }
    };

    Verification {
        dirty_files,
        unpushed_commits,
        unverified: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{FakeRunner, Output};

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn clean_and_pushed_repository() {
        let runner = FakeRunner::new(|spec| {
            if spec.args.contains(&"status".to_string()) {
                Output {
                    code: Some(0),
                    stdout: String::new(),
                    ..Output::default()
                }
            } else if spec.args.contains(&"@{u}".to_string()) {
                Output {
                    code: Some(0),
                    stdout: "origin/main\n".into(),
                    ..Output::default()
                }
            } else {
                Output {
                    code: Some(0),
                    stdout: "0\n".into(),
                    ..Output::default()
                }
            }
        });
        let result = verify(&runner, &cfg(), Path::new("/tmp/x"));
        assert!(result.is_clean());
        assert_eq!(result.summary(), "clean and pushed");
    }

    #[test]
    fn detects_dirty_tree() {
        let runner = FakeRunner::new(|spec| {
            if spec.args.contains(&"status".to_string()) {
                Output {
                    code: Some(0),
                    stdout: " M src/main.rs\n?? notes.txt\n".into(),
                    ..Output::default()
                }
            } else if spec.args.contains(&"@{u}".to_string()) {
                Output {
                    code: Some(0),
                    stdout: "origin/main\n".into(),
                    ..Output::default()
                }
            } else {
                Output {
                    code: Some(0),
                    stdout: "0\n".into(),
                    ..Output::default()
                }
            }
        });
        let result = verify(&runner, &cfg(), Path::new("/tmp/x"));
        assert_eq!(result.dirty_files, 2);
        assert!(!result.is_clean());
        assert!(result.reason().unwrap().contains("2 uncommitted"));
    }

    #[test]
    fn detects_unpushed_commits_via_origin_main() {
        let runner = FakeRunner::new(|spec| {
            if spec.args.contains(&"status".to_string()) {
                return Output::ok();
            }
            if spec.args.contains(&"@{u}".to_string()) {
                return Output::failure(128, "no upstream configured");
            }
            if spec.args.iter().any(|a| a == "origin/main") {
                return Output {
                    code: Some(0),
                    stdout: "abc123\n".into(),
                    ..Output::default()
                };
            }
            Output {
                code: Some(0),
                stdout: "3\n".into(),
                ..Output::default()
            }
        });
        let result = verify(&runner, &cfg(), Path::new("/tmp/x"));
        assert_eq!(result.unpushed_commits, 3);
        assert!(result.reason().unwrap().contains("3 unpushed"));
    }

    #[test]
    fn reports_missing_upstream_as_unverified_not_failed() {
        let runner = FakeRunner::new(|spec| {
            if spec.args.contains(&"status".to_string()) {
                return Output::ok();
            }
            Output::failure(128, "not found")
        });
        let result = verify(&runner, &cfg(), Path::new("/tmp/x"));
        assert!(result.is_clean());
        assert!(result.unverified.is_some());
        assert!(result.summary().contains("no upstream"));
    }

    #[test]
    fn reports_git_status_failure() {
        let runner = FakeRunner::new(|_| Output::failure(128, "not a git repository"));
        let result = verify(&runner, &cfg(), Path::new("/tmp/x"));
        assert!(result.unverified.unwrap().contains("git status failed"));
    }
}
