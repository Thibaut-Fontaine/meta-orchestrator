//! Configuration: defaults, TOML file, and environment overrides.
//!
//! Precedence: built-in defaults < `~/.config/meta-orchestrator/config.toml` <
//! `MOX_*` environment variables < explicit CLI flags.

use crate::util::{expand_tilde, home_dir};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// All tunables of the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where project workspaces live.
    pub projects_root: String,
    /// Where `state.json` and the cached plan live.
    pub state_dir: String,
    /// OpenCode executable.
    pub opencode_bin: String,
    /// GitHub CLI executable.
    pub gh_bin: String,
    /// Git executable.
    pub git_bin: String,
    /// Optional `taskset` wrapper (CPU pinning). Empty disables pinning.
    pub taskset_bin: String,
    /// Optional `nice` wrapper.
    pub nice_bin: String,
    /// Optional `ionice` wrapper (idle IO class).
    pub ionice_bin: String,
    /// SSH executable for remote execution.
    pub ssh_bin: String,
    /// rsync executable for mirroring projects to the VPS.
    pub rsync_bin: String,
    /// Remote root on the VPS (tilde allowed).
    pub remote_root: String,
    /// OpenCode executable on the VPS.
    pub remote_opencode_bin: String,
    /// SSH alias of the VPS (from ~/.ssh/config).
    pub ssh_alias: String,
    /// Global agent rules the agents are told to read.
    pub agents_global: String,
    /// Maximum number of OpenCode agents running at once.
    pub max_parallel: usize,
    /// Cores left free for the OS/UI.
    pub reserve_cores: usize,
    /// `nice` level applied to agents.
    pub nice_level: i32,
    /// Refuse to start work below this much free disk.
    pub min_free_disk_mb: u64,
    /// Assumed RAM per running agent (admission control).
    pub mem_per_agent_mb: u64,
    /// Base delay before retrying a failed project (doubles per attempt, max 60s).
    pub retry_backoff_secs: u64,
    /// Check free disk during local runs; 0 disables.
    pub disk_watchdog_secs: u64,
    /// Maximum launch attempts before `--force` is required.
    pub max_attempts: u32,
    /// Write an automatic reflection after a successful run (uses the brain).
    pub reflect_after_run: bool,
    /// Soft budget per project, used by `status` and `clean`.
    pub project_disk_budget_mb: u64,
    /// Timeout for one brainstorming call.
    pub brain_timeout_secs: u64,
    /// Timeout for one project run.
    pub run_timeout_secs: u64,
    /// Optional `provider/model` passed to opencode.
    pub model: String,
    /// Auto-approve OpenCode permissions (unattended runs need this).
    pub auto_approve: bool,
    /// Run each agent with its own private server instead of the shared one.
    /// More isolation, more RAM; default keeps RAM low.
    pub standalone: bool,
    /// Extra instructions appended to every project prompt.
    pub extra_instructions: String,

    /// Desktop notifications via `notify-send`.
    pub notify_desktop: bool,
    /// Telegram bot notifications. The token is read from an env var.
    pub notify_telegram: bool,
    /// Hard wall: allow the mox Telegram bot to run on a remote/VPS host.
    /// Default false; only set by a human who explicitly wants it.
    pub allow_remote_bots: bool,
    pub telegram_chat_id: String,
    pub telegram_token_env: String,
    /// Generic JSON webhook notifications.
    pub notify_webhook: bool,
    pub webhook_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            projects_root: "~/mox-projects".to_string(),
            state_dir: "~/.local/state/meta-orchestrator".to_string(),
            opencode_bin: "opencode".to_string(),
            gh_bin: "gh".to_string(),
            git_bin: "git".to_string(),
            taskset_bin: "taskset".to_string(),
            nice_bin: "nice".to_string(),
            ionice_bin: "ionice".to_string(),
            ssh_bin: "ssh".to_string(),
            rsync_bin: "rsync".to_string(),
            remote_root: "~/mox-remote".to_string(),
            remote_opencode_bin: "opencode".to_string(),
            ssh_alias: "vps-ovh".to_string(),
            agents_global: "~/AGENTS-GLOBAL.md".to_string(),
            max_parallel: default_max_parallel(),
            reserve_cores: 1,
            nice_level: 10,
            min_free_disk_mb: 2048,
            mem_per_agent_mb: 768,
            retry_backoff_secs: 5,
            disk_watchdog_secs: 30,
            max_attempts: 3,
            reflect_after_run: false,
            project_disk_budget_mb: 4096,
            brain_timeout_secs: 900,
            run_timeout_secs: 7200,
            model: String::new(),
            auto_approve: true,
            // A private server per agent inherits PATH/TMPDIR/CARGO_TARGET_DIR,
            // which is what makes guardrails and cache isolation effective.
            standalone: true,
            extra_instructions: String::new(),
            notify_desktop: false,
            notify_telegram: false,
            allow_remote_bots: false,
            telegram_chat_id: String::new(),
            telegram_token_env: "MOX_TELEGRAM_TOKEN".to_string(),
            notify_webhook: false,
            webhook_url: String::new(),
        }
    }
}

impl Config {
    /// Load configuration from `path` (or the default location), applying
    /// environment overrides. A missing file is not an error.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        };
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?
        } else {
            Config::default()
        };
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply `MOX_*` environment overrides. Used both by the binary and tests.
    pub fn apply_env(&mut self) {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
        if let Some(v) = var("MOX_PROJECTS_ROOT") {
            self.projects_root = v;
        }
        if let Some(v) = var("MOX_STATE_DIR") {
            self.state_dir = v;
        }
        if let Some(v) = var("MOX_OPENCODE_BIN") {
            self.opencode_bin = v;
        }
        if let Some(v) = var("MOX_GH_BIN") {
            self.gh_bin = v;
        }
        if let Some(v) = var("MOX_GIT_BIN") {
            self.git_bin = v;
        }
        if let Some(v) = var("MOX_TASKSET_BIN") {
            self.taskset_bin = v;
        }
        if let Some(v) = var("MOX_NICE_BIN") {
            self.nice_bin = v;
        }
        if let Some(v) = var("MOX_IONICE_BIN") {
            self.ionice_bin = v;
        }
        if let Some(v) = var("MOX_SSH_BIN") {
            self.ssh_bin = v;
        }
        if let Some(v) = var("MOX_RSYNC_BIN") {
            self.rsync_bin = v;
        }
        if let Some(v) = var("MOX_REMOTE_ROOT") {
            self.remote_root = v;
        }
        if let Some(v) = var("MOX_REMOTE_OPENCODE_BIN") {
            self.remote_opencode_bin = v;
        }
        if let Some(v) = var("MOX_SSH_ALIAS") {
            self.ssh_alias = v;
        }
        if let Some(v) = var("MOX_AGENTS_GLOBAL") {
            self.agents_global = v;
        }
        if let Some(v) = var("MOX_MAX_PARALLEL") {
            if let Ok(n) = v.parse() {
                self.max_parallel = n;
            }
        }
        if let Some(v) = var("MOX_RESERVE_CORES") {
            if let Ok(n) = v.parse() {
                self.reserve_cores = n;
            }
        }
        if let Some(v) = var("MOX_REFLECT_AFTER_RUN") {
            self.reflect_after_run = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = var("MOX_DISK_WATCHDOG_SECS") {
            if let Ok(n) = v.parse() {
                self.disk_watchdog_secs = n;
            }
        }
        if let Some(v) = var("MOX_RETRY_BACKOFF_SECS") {
            if let Ok(n) = v.parse() {
                self.retry_backoff_secs = n;
            }
        }
        if let Some(v) = var("MOX_MIN_FREE_DISK_MB") {
            if let Ok(n) = v.parse() {
                self.min_free_disk_mb = n;
            }
        }
        if let Some(v) = var("MOX_PROJECT_DISK_BUDGET_MB") {
            if let Ok(n) = v.parse() {
                self.project_disk_budget_mb = n;
            }
        }
        if let Some(v) = var("MOX_MODEL") {
            self.model = v;
        }
        if let Some(v) = var("MOX_AUTO_APPROVE") {
            self.auto_approve = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = var("MOX_ALLOW_REMOTE_BOTS") {
            self.allow_remote_bots = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = var("MOX_STANDALONE") {
            self.standalone = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = var("MOX_RUN_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.run_timeout_secs = n;
            }
        }
    }

    pub fn validate(&mut self) -> Result<()> {
        if self.max_parallel == 0 {
            self.max_parallel = 1;
        }
        if self.min_free_disk_mb == 0 {
            self.min_free_disk_mb = 1;
        }
        if self.projects_root.trim().is_empty() || self.state_dir.trim().is_empty() {
            anyhow::bail!("projects_root and state_dir must not be empty");
        }
        Ok(())
    }

    pub fn projects_root_path(&self) -> PathBuf {
        expand_tilde(&self.projects_root)
    }

    pub fn state_dir_path(&self) -> PathBuf {
        expand_tilde(&self.state_dir)
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir_path().join("state.json")
    }

    pub fn plan_file(&self) -> PathBuf {
        self.state_dir_path().join("plan.json")
    }

    pub fn answers_file(&self) -> PathBuf {
        self.state_dir_path().join("answers.toml")
    }

    /// Global human-attention queue (`mox check`).
    pub fn checks_file(&self) -> PathBuf {
        self.state_dir_path().join("checks.json")
    }

    /// Cross-project milestone timing statistics.
    pub fn stats_file(&self) -> PathBuf {
        self.state_dir_path().join("stats.json")
    }

    pub fn questions_file(&self) -> PathBuf {
        self.state_dir_path().join("QUESTIONS.md")
    }

    /// Internal directory for logs, pid files and shared caches. Lives outside
    /// every repository so build output is never committed.
    pub fn internal_dir(&self) -> PathBuf {
        self.projects_root_path().join(".mox")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.internal_dir().join("logs")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.internal_dir().join("run")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.internal_dir().join("cache")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.internal_dir().join("tmp")
    }

    pub fn agents_global_path(&self) -> PathBuf {
        expand_tilde(&self.agents_global)
    }

    /// Cargo target dir for one project: shared cache root, per project, never
    /// inside the repository.
    pub fn cargo_target_for(&self, slug: &str) -> PathBuf {
        self.cache_dir().join("cargo-target").join(slug)
    }

    pub fn tmp_for(&self, slug: &str) -> PathBuf {
        self.tmp_dir().join(slug)
    }

    /// Serialise the config as a commented TOML document.
    pub fn to_toml(&self) -> String {
        format!(
            r#"# meta-orchestrator configuration
# Regenerate with `mox init --force`. Every value can be overridden with MOX_* env vars.

# Where project workspaces live (each one is an independent git repo).
projects_root = {projects_root:?}
# Where orchestration state lives (state.json, plan.json, answers.toml).
state_dir = {state_dir:?}

# Executables.
opencode_bin = {opencode_bin:?}
gh_bin = {gh_bin:?}
git_bin = {git_bin:?}
taskset_bin = {taskset_bin:?}
nice_bin = {nice_bin:?}
ionice_bin = {ionice_bin:?}
ssh_bin = {ssh_bin:?}
rsync_bin = {rsync_bin:?}
remote_root = {remote_root:?}
remote_opencode_bin = {remote_opencode_bin:?}

# VPS access (read-only checks by default, see `mox vps check`).
ssh_alias = {ssh_alias:?}
agents_global = {agents_global:?}

# Parallelism. max_parallel agents run at once, each pinned to distinct cores.
max_parallel = {max_parallel}
reserve_cores = {reserve_cores}
nice_level = {nice_level}

# Disk safety. Work refuses to start below min_free_disk_mb.
min_free_disk_mb = {min_free_disk_mb}
mem_per_agent_mb = {mem_per_agent_mb}
retry_backoff_secs = {retry_backoff_secs}
disk_watchdog_secs = {disk_watchdog_secs}
max_attempts = {max_attempts}
reflect_after_run = {reflect_after_run}
project_disk_budget_mb = {project_disk_budget_mb}

# Timeouts.
brain_timeout_secs = {brain_timeout_secs}
run_timeout_secs = {run_timeout_secs}

# Optional provider/model for opencode, e.g. "anthropic/claude-sonnet-4".
model = {model:?}
auto_approve = {auto_approve}
standalone = {standalone}
extra_instructions = {extra_instructions:?}

# Notifications (optional). Telegram token stays in the environment:
#   export MOX_TELEGRAM_TOKEN=...   (from @BotFather)
notify_desktop = {notify_desktop}
notify_telegram = {notify_telegram}
allow_remote_bots = {allow_remote_bots}
telegram_chat_id = {telegram_chat_id:?}
telegram_token_env = {telegram_token_env:?}
notify_webhook = {notify_webhook}
webhook_url = {webhook_url:?}
"#,
            projects_root = self.projects_root,
            state_dir = self.state_dir,
            opencode_bin = self.opencode_bin,
            gh_bin = self.gh_bin,
            git_bin = self.git_bin,
            taskset_bin = self.taskset_bin,
            nice_bin = self.nice_bin,
            ionice_bin = self.ionice_bin,
            ssh_bin = self.ssh_bin,
            rsync_bin = self.rsync_bin,
            remote_root = self.remote_root,
            remote_opencode_bin = self.remote_opencode_bin,
            ssh_alias = self.ssh_alias,
            agents_global = self.agents_global,
            max_parallel = self.max_parallel,
            reserve_cores = self.reserve_cores,
            nice_level = self.nice_level,
            min_free_disk_mb = self.min_free_disk_mb,
            mem_per_agent_mb = self.mem_per_agent_mb,
            retry_backoff_secs = self.retry_backoff_secs,
            disk_watchdog_secs = self.disk_watchdog_secs,
            max_attempts = self.max_attempts,
            reflect_after_run = self.reflect_after_run,
            project_disk_budget_mb = self.project_disk_budget_mb,
            brain_timeout_secs = self.brain_timeout_secs,
            run_timeout_secs = self.run_timeout_secs,
            model = self.model,
            auto_approve = self.auto_approve,
            standalone = self.standalone,
            extra_instructions = self.extra_instructions,
            notify_desktop = self.notify_desktop,
            notify_telegram = self.notify_telegram,
            allow_remote_bots = self.allow_remote_bots,
            telegram_chat_id = self.telegram_chat_id,
            telegram_token_env = self.telegram_token_env,
            notify_webhook = self.notify_webhook,
            webhook_url = self.webhook_url,
        )
    }

    /// Write the config file with mode 0600 (it may reference paths but should
    /// still not be world-readable).
    pub fn write(&self, path: &Path, force: bool) -> Result<bool> {
        if path.exists() && !force {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_toml())
            .with_context(|| format!("writing config {}", path.display()))?;
        set_private_permissions(path)?;
        Ok(true)
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// `~/.config/meta-orchestrator/config.toml`, or `$MOX_CONFIG`.
pub fn default_config_path() -> PathBuf {
    if let Ok(path) = std::env::var("MOX_CONFIG") {
        if !path.is_empty() {
            return expand_tilde(&path);
        }
    }
    home_dir()
        .join(".config")
        .join("meta-orchestrator")
        .join("config.toml")
}

fn default_max_parallel() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Keep at least two cores per agent so it can compile while it thinks.
    (cores / 2).clamp(1, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let cfg = Config::default();
        assert_eq!(cfg.projects_root, "~/mox-projects");
        assert!(cfg.max_parallel >= 1);
        assert!(cfg.min_free_disk_mb >= 1024);
        assert!(cfg.auto_approve);
        assert!(cfg.standalone);
        assert_eq!(cfg.ssh_alias, "vps-ovh");
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = Config {
            max_parallel: 3,
            min_free_disk_mb: 1234,
            projects_root: "/tmp/p".into(),
            ..Config::default()
        };
        let text = cfg.to_toml();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn deny_unknown_fields() {
        let err = toml::from_str::<Config>("max_parallel = 2\nbogus = 1\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn load_missing_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.projects_root, Config::default().projects_root);
    }

    #[test]
    fn validates_zero_parallel() {
        let mut cfg = Config {
            max_parallel: 0,
            ..Config::default()
        };
        cfg.validate().unwrap();
        assert_eq!(cfg.max_parallel, 1);
    }

    #[test]
    fn writes_private_file_and_respects_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::default();
        assert!(cfg.write(&path, false).unwrap());
        assert!(!cfg.write(&path, false).unwrap());
        assert!(cfg.write(&path, true).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn internal_paths_stay_outside_project_dirs() {
        let cfg = Config {
            projects_root: "/tmp/root".into(),
            state_dir: "/tmp/state".into(),
            ..Config::default()
        };
        assert_eq!(cfg.internal_dir(), PathBuf::from("/tmp/root/.mox"));
        assert!(cfg
            .cargo_target_for("foo")
            .starts_with("/tmp/root/.mox/cache"));
    }
}
