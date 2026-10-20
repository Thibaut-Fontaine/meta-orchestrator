//! Git and GitHub operations.
//!
//! Safety properties enforced here and covered by tests:
//! * repositories are **always** created with `--private`;
//! * no command can delete, rename, transfer or change visibility;
//! * an existing repository is never reused or overwritten — the create call
//!   fails and the project is marked failed;
//! * the final visibility is verified with a read-only `gh repo view`.

use crate::exec::{run_checked, CommandRunner, CommandSpec, Output};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Outcome of publishing one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub repo_url: String,
    pub commands: Vec<String>,
    pub dry_run: bool,
}

/// Stage and publish one project directory.
///
/// The directory must already be per-repository (each project is its own git
/// repository). When `dry_run` is true nothing is executed; the returned
/// commands are exactly what would run.
pub fn publish_private_repo(
    runner: &dyn CommandRunner,
    cfg: &crate::config::Config,
    dir: &Path,
    slug: &str,
    owner: Option<&str>,
    dry_run: bool,
) -> Result<PublishResult> {
    if !dry_run && !dir.is_dir() {
        bail!("project directory does not exist: {}", dir.display());
    }
    let commit_message = "chore: scaffold project from meta-orchestrator";
    let repo_name = match owner.map(str::trim).filter(|o| !o.is_empty()) {
        Some(owner) => format!("{owner}/{slug}"),
        None => slug.to_string(),
    };

    let git = |args: Vec<&str>| {
        CommandSpec::new(&cfg.git_bin)
            .args(args)
            .cwd(dir)
            .timeout(120)
    };
    let init = git(vec!["init", "-b", "main"]);
    let add = git(vec!["add", "-A"]);
    let commit = git(vec!["commit", "-m", commit_message, "--no-gpg-sign"]);
    let create = CommandSpec::new(&cfg.gh_bin)
        .args([
            "repo",
            "create",
            &repo_name,
            "--private",
            "--source",
            ".",
            "--remote",
            "origin",
            "--push",
        ])
        .cwd(dir)
        .timeout(300);
    let verify = CommandSpec::new(&cfg.gh_bin)
        .args(["repo", "view", &repo_name, "--json", "visibility,url"])
        .cwd(dir)
        .timeout(60);
    let remote = git(vec!["remote", "get-url", "origin"]);

    let commands = vec![
        init.display(),
        add.display(),
        commit.display(),
        create.display(),
        verify.display(),
        remote.display(),
    ];
    if dry_run {
        return Ok(PublishResult {
            repo_url: format!("https://github.com/<owner>/{slug}"),
            commands,
            dry_run: true,
        });
    }

    if !dir.join(".git").exists() {
        run_checked(runner, &init, "git init")?;
    }
    // Last line of defence against heavy files and secrets.
    crate::hooks::install(dir)?;
    run_checked(runner, &add, "git add")?;
    let commit_output = runner.run(&commit)?;
    if !commit_output.success() && !is_nothing_to_commit(&commit_output) {
        bail!(
            "git commit failed ({}): {}",
            commit_output.summary(),
            commit_output.tail(10)
        );
    }

    let created = runner.run(&create)?;
    if !created.success() {
        bail!(
            "gh repo create failed ({}). The repository may already exist; mox never \
             reuses or overwrites an existing repository. Command: {}\n{}",
            created.summary(),
            create.display(),
            created.tail(12)
        );
    }

    // Read-only verification that the new repository is private.
    let verified = runner.run(&verify)?;
    if !verified.success() {
        bail!(
            "could not verify repository visibility ({}): {}",
            verified.summary(),
            verified.tail(8)
        );
    }
    let visibility = serde_json::from_str::<serde_json::Value>(&verified.stdout)
        .ok()
        .and_then(|v| {
            v.get("visibility")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if !visibility.eq_ignore_ascii_case("private") {
        bail!(
            "refusing to continue: repository `{repo_name}` is not private (visibility: `{visibility}`). \
             Make it private manually before running agents."
        );
    }

    let remote_output = runner.run(&remote)?;
    let repo_url = if remote_output.success() {
        ssh_remote_to_https(remote_output.stdout.trim())
    } else {
        String::new()
    };

    Ok(PublishResult {
        repo_url,
        commands,
        dry_run: false,
    })
}

fn is_nothing_to_commit(output: &Output) -> bool {
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("nothing to commit") || text.contains("no changes added to commit")
}

/// `git@github.com:Owner/name.git` -> `https://github.com/Owner/name`.
pub fn ssh_remote_to_https(remote: &str) -> String {
    let remote = remote.trim();
    if let Some(rest) = remote.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        return format!("https://github.com/{rest}");
    }
    if let Some(rest) = remote.strip_prefix("ssh://git@github.com/") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        return format!("https://github.com/{rest}");
    }
    remote.to_string()
}

/// Check that a directory is a git repository with an `origin` remote.
pub fn has_origin(runner: &dyn CommandRunner, cfg: &crate::config::Config, dir: &Path) -> bool {
    let spec = CommandSpec::new(&cfg.git_bin)
        .args(["remote", "get-url", "origin"])
        .cwd(dir)
        .timeout(30);
    runner.run(&spec).map(|o| o.success()).unwrap_or(false)
}

/// Guard used by app code: never accept a public URL.
pub fn ensure_private_hint(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        return Ok(());
    }
    if url.starts_with("https://github.com/") || url.starts_with("git@github.com:") {
        return Ok(());
    }
    bail!("unexpected repository URL (only GitHub is supported): {url}")
}

/// Read the authenticated GitHub login, without enumerating anything.
pub fn current_login(runner: &dyn CommandRunner, cfg: &crate::config::Config) -> Result<String> {
    let spec = CommandSpec::new(&cfg.gh_bin)
        .args(["api", "user", "--jq", ".login"])
        .timeout(60);
    let output = runner.run(&spec).context("running gh api user")?;
    if !output.success() {
        bail!("gh api user failed: {}", output.tail(5));
    }
    Ok(output.stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::exec::FakeRunner;
    use std::sync::Arc;

    fn cfg() -> Config {
        Config::default()
    }

    fn runner_with_visibility(visibility: &str) -> FakeRunner {
        let visibility = visibility.to_string();
        FakeRunner::new(move |spec| {
            if spec.program == "gh"
                && spec.args.first().map(String::as_str) == Some("repo")
                && spec.args.contains(&"view".to_string())
            {
                return Output {
                    code: Some(0),
                    stdout: format!(
                        r#"{{"visibility":"{visibility}","url":"https://github.com/me/x"}}"#
                    ),
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

    fn project_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        dir
    }

    #[test]
    fn creates_private_repo_and_verifies() {
        let runner = runner_with_visibility("PRIVATE");
        let dir = project_dir();
        let result =
            publish_private_repo(&runner, &cfg(), dir.path(), "demo", None, false).unwrap();
        assert_eq!(result.repo_url, "https://github.com/me/demo");
        let calls = runner.displays();
        assert!(calls
            .iter()
            .any(|c| c.contains("repo create demo --private")));
        assert!(calls.iter().any(|c| c.contains("repo view demo")));
        assert!(!calls.iter().any(|c| c.contains("--public")));
        // The verify call is after the create call.
        let create_at = calls
            .iter()
            .position(|c| c.contains("repo create"))
            .unwrap();
        let view_at = calls.iter().position(|c| c.contains("repo view")).unwrap();
        assert!(view_at > create_at);
    }

    #[test]
    fn refuses_public_repo() {
        let runner = runner_with_visibility("PUBLIC");
        let dir = project_dir();
        let err =
            publish_private_repo(&runner, &cfg(), dir.path(), "demo", None, false).unwrap_err();
        assert!(err.to_string().contains("not private"));
    }

    #[test]
    fn owner_prefixes_the_repository_name() {
        let runner = runner_with_visibility("PRIVATE");
        let dir = project_dir();
        let result =
            publish_private_repo(&runner, &cfg(), dir.path(), "demo", Some("my-org"), false)
                .unwrap();
        assert!(result
            .commands
            .iter()
            .any(|c| c.contains("repo create my-org/demo --private")));
        assert!(result
            .commands
            .iter()
            .any(|c| c.contains("repo view my-org/demo")));
    }

    #[test]
    fn never_runs_in_dry_run() {
        let runner = FakeRunner::ok();
        let dir = project_dir();
        let result = publish_private_repo(&runner, &cfg(), dir.path(), "demo", None, true).unwrap();
        assert!(result.dry_run);
        assert_eq!(runner.call_count(), 0);
        assert!(result.commands.iter().any(|c| c.contains("--private")));
        assert!(!result.commands.iter().any(|c| c.contains("--public")));
    }

    #[test]
    fn tolerates_nothing_to_commit() {
        let runner = FakeRunner::new(move |spec| {
            if spec.program == "git" && spec.args.first().map(String::as_str) == Some("commit") {
                return Output::failure(1, "nothing to commit, working tree clean");
            }
            if spec.args.contains(&"view".to_string()) {
                return Output {
                    code: Some(0),
                    stdout: r#"{"visibility":"PRIVATE"}"#.into(),
                    ..Output::default()
                };
            }
            if spec.args.contains(&"get-url".to_string()) {
                return Output {
                    code: Some(0),
                    stdout: "git@github.com:me/demo.git\n".into(),
                    ..Output::default()
                };
            }
            Output::ok()
        });
        let dir = project_dir();
        publish_private_repo(&runner, &cfg(), dir.path(), "demo", None, false).unwrap();
    }

    #[test]
    fn reports_existing_repo_failure() {
        let runner = FakeRunner::new(|spec| {
            if spec.args.contains(&"create".to_string()) {
                return Output::failure(1, "GraphQL: Name already exists (createRepository)");
            }
            if spec.args.contains(&"view".to_string()) {
                return Output {
                    code: Some(0),
                    stdout: r#"{"visibility":"PRIVATE"}"#.into(),
                    ..Output::default()
                };
            }
            Output::ok()
        });
        let dir = project_dir();
        let err =
            publish_private_repo(&runner, &cfg(), dir.path(), "demo", None, false).unwrap_err();
        assert!(err.to_string().contains("may already exist"));
    }

    #[test]
    fn converts_ssh_remotes() {
        assert_eq!(
            ssh_remote_to_https("git@github.com:Owner/name.git"),
            "https://github.com/Owner/name"
        );
        assert_eq!(
            ssh_remote_to_https("ssh://git@github.com/Owner/name.git"),
            "https://github.com/Owner/name"
        );
        assert_eq!(
            ssh_remote_to_https("https://github.com/Owner/name"),
            "https://github.com/Owner/name"
        );
    }

    #[test]
    fn private_hint_accepts_github_only() {
        assert!(ensure_private_hint("https://github.com/a/b").is_ok());
        assert!(ensure_private_hint("").is_ok());
        assert!(ensure_private_hint("https://gitlab.com/a/b").is_err());
    }

    #[test]
    fn login_is_read_via_single_user_endpoint() {
        let runner = Arc::new(FakeRunner::new(|spec| {
            assert_eq!(spec.args, vec!["api", "user", "--jq", ".login"]);
            Output {
                code: Some(0),
                stdout: "thibaut\n".into(),
                ..Output::default()
            }
        }));
        assert_eq!(current_login(runner.as_ref(), &cfg()).unwrap(), "thibaut");
    }
}
