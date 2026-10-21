//! Command-line interface definition (clap).

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "mox",
    version,
    about = "Parallel OpenCode project orchestrator",
    long_about = "Turns project ideas into clarifying questions, specs, private GitHub \
                  repositories and isolated parallel OpenCode agents.\n\n\
                  Typical flow:\n  mox plan --ideas ideas.txt\n  mox up\n  mox status"
)]
pub struct Cli {
    /// Config file (default: ~/.config/meta-orchestrator/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Override the projects root directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub projects_root: Option<PathBuf>,

    /// Show what would happen without creating files or running commands.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Wall-clock cap for one agent run, in seconds (default: config value).
    #[arg(long, global = true, value_name = "SECS")]
    pub run_timeout: Option<u64>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the config file and the projects/cache directories.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
        /// Configuration preset.
        #[arg(long, default_value = "standard", value_parser = ["minimal", "standard", "paranoid"])]
        profile: String,
    },

    /// Check the local environment (tools, auth, disk, VPS, cores).
    Doctor {
        /// Skip the SSH probe.
        #[arg(long)]
        skip_vps: bool,
    },

    /// Turn ideas into questions and a written plan.
    Plan {
        /// Ideas file (`-` for stdin).
        #[arg(long, value_name = "FILE")]
        ideas: Option<PathBuf>,

        /// Answers file to apply (`answers.toml`).
        #[arg(long, value_name = "FILE")]
        answers: Option<PathBuf>,

        /// Ask the brain again instead of reusing the cached plan.
        #[arg(long)]
        refresh: bool,

        /// Do not ask questions interactively; use defaults.
        #[arg(long)]
        yes: bool,

        /// Force interactive questions even when stdin is not a TTY.
        #[arg(long)]
        interactive: bool,

        /// Second planning pass: refine the spec from the answers.
        #[arg(long)]
        deep: bool,

        /// Mark the existing plan as approved for execution.
        #[arg(long)]
        approve: bool,
    },

    /// Create workspaces, private GitHub repos, then run agents in parallel.
    Up {
        /// Only handle these slugs (repeatable). Default: every project.
        #[arg(long = "only", value_name = "SLUG")]
        only: Vec<String>,

        /// Override the number of simultaneous agents.
        #[arg(long, value_name = "N")]
        max_parallel: Option<usize>,

        /// Do not create or push GitHub repositories.
        #[arg(long)]
        no_github: bool,

        /// Scaffold and publish only; do not launch agents.
        #[arg(long)]
        no_run: bool,

        /// Ignore the free-disk floor.
        #[arg(long)]
        force: bool,

        /// Resume unfinished projects (skip completed ones).
        #[arg(long)]
        resume: bool,

        /// Retry only projects that failed or were stopped.
        #[arg(long)]
        retry_failed: bool,

        /// Where to run: local, vps (needs approval) or auto.
        #[arg(long, default_value = "local", value_parser = ["local", "vps", "auto"])]
        target: String,

        /// GitHub owner/org for new repositories (default: authenticated user).
        #[arg(long, value_name = "OWNER")]
        owner: Option<String>,

        /// Start VPS runs detached (survives shutting this computer down).
        #[arg(long)]
        detach: bool,

        /// Skip the plan-approval prompt and approve the plan (automation).
        #[arg(long)]
        yes: bool,
    },

    /// Show the state of every project, with milestone progress and ETA.
    Status {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,

        /// Refresh continuously (Ctrl-C to quit).
        #[arg(long)]
        watch: bool,

        /// Refresh interval in seconds.
        #[arg(long, default_value_t = 10)]
        interval: u64,

        /// Include the first lines of each project's STATUS.md.
        #[arg(long)]
        verbose: bool,
    },

    /// Print the tail of a project's agent log.
    Logs {
        /// Project slug.
        slug: String,

        /// Number of lines.
        #[arg(short = 'n', long, default_value_t = 60)]
        lines: usize,
    },

    /// Review one finished run and write REFLECTION.md.
    Reflect {
        /// Project slug.
        slug: String,

        /// Log lines to include in the reflection prompt.
        #[arg(long, default_value_t = 80)]
        lines: usize,
    },

    /// Report or remove caches and build output.
    Clean {
        /// Actually delete files (default is a dry run).
        #[arg(long)]
        apply: bool,

        /// Also delete build output (target/, dist/, build/).
        #[arg(long)]
        all: bool,

        /// Delete even if an agent might be running.
        #[arg(long)]
        force: bool,
        /// Also delete logs/prompts older than this many days.
        #[arg(long, value_name = "DAYS")]
        logs_older_than_days: Option<u64>,
    },

    /// Stop a running agent.
    Stop {
        /// Project slug.
        slug: String,
    },

    /// Human attention queue: what to check now, what later, and when.
    #[command(alias = "checks", alias = "attention")]
    Check {
        #[command(subcommand)]
        action: Option<CheckAction>,

        /// Machine-readable output.
        #[arg(long)]
        json: bool,

        /// One line, for a status bar.
        #[arg(long)]
        oneline: bool,

        /// Refresh the dashboard continuously (Ctrl-C to quit).
        #[arg(long)]
        watch: bool,

        /// Refresh interval in seconds.
        #[arg(long, default_value_t = 10)]
        interval: u64,

        /// Include completed checks.
        #[arg(long)]
        all: bool,

        /// Do not import CHECKS.md files from projects.
        #[arg(long)]
        no_sync: bool,

        /// Show only the first N open checks.
        #[arg(long, value_name = "N")]
        top: Option<usize>,

        /// Append project path and short commit after each check.
        #[arg(long)]
        context: bool,
    },

    /// Track milestones, progress and ETA for the project in the current dir.
    Milestone {
        /// Board file (default: ./MILESTONES.json).
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,

        #[command(subcommand)]
        action: Option<MilestoneAction>,
    },

    /// Project lifecycle: list or archive workspaces.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },

    /// Print a shell completion script.
    Completions {
        /// bash, zsh or fish.
        #[arg(value_parser = ["bash", "zsh", "fish"])]
        shell: String,
    },

    /// Disaster-recovery snapshots (local + VPS RECOVERY.md).
    Recovery {
        /// Push the snapshots to the VPS.
        #[arg(long)]
        push: bool,
        /// Check the age of the recovery files on the VPS.
        #[arg(long)]
        check: bool,
        /// Only this project slug.
        #[arg(long)]
        slug: Option<String>,
    },

    /// Show the local event log.
    Events {
        /// Number of trailing lines.
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },

    /// Write a Markdown report across all projects.
    Report {
        /// Write to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },

    /// Cross-project milestone timing statistics (used to improve new ETAs).
    Stats {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Delete the learned cross-project statistics.
        #[arg(long)]
        reset: bool,
        /// Confirm the reset.
        #[arg(long)]
        yes: bool,
    },

    /// Read-only Telegram bot for projects, milestones, todos, checks and ETA.
    Bot {
        /// Process one long-poll batch and exit (debug/cron).
        #[arg(long)]
        once: bool,

        /// Explicitly allow the bot to run on a remote/VPS checkout.
        /// mox never sets this automatically.
        #[arg(long)]
        allow_remote: bool,
    },

    /// Send or inspect notifications (Telegram, desktop, webhook).
    Notify {
        #[command(subcommand)]
        action: NotifyAction,
    },

    /// Check the linked DeepSeek account balance.
    Balance {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,

        /// Send a Telegram notification for each newly crossed threshold.
        #[arg(long)]
        notify: bool,
    },

    /// Inspect the VPS or get an execution-target recommendation.
    Vps {
        #[command(subcommand)]
        action: VpsCommand,
    },

    /// Parse and validate an ideas file without planning.
    Ideas {
        /// Ideas file (`-` for stdin).
        file: PathBuf,
    },

    /// Internal: enforce agent guardrails, then exec the real program.
    #[command(name = "__guard", hide = true)]
    Guard {
        /// Wrapped program name (git, gh, ssh, ...).
        program: String,
        /// Original arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MilestoneAction {
    /// Progress and ETA (default when no action is given).
    Status,
    /// List milestones and their open todos.
    List,
    /// Mark a milestone as in progress.
    Start {
        /// Milestone id, e.g. m1.
        id: String,
    },
    /// Mark a milestone as done (records actual time).
    Done {
        /// Milestone id.
        id: String,
        /// Short outcome note.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Mark a milestone as blocked; the ETA becomes unavailable.
    Block {
        /// Milestone id.
        id: String,
        /// Why it is blocked.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Skip a milestone (excluded from progress/ETA).
    Skip {
        /// Milestone id.
        id: String,
    },
    /// Tick or untick a sub-todo (1-based index).
    Todo {
        /// Milestone id.
        id: String,
        /// 1-based todo index (see `mox milestone list`).
        index: usize,
        /// Reopen the todo instead of marking it done.
        #[arg(long)]
        undo: bool,
    },
    /// Add a sub-todo to a milestone.
    AddTodo {
        /// Milestone id.
        id: String,
        /// Todo text.
        text: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectAction {
    /// List projects (same table as `mox status`).
    List,
    /// Move a finished workspace under .mox/archive (never deletes the repo).
    Archive {
        /// Project slug.
        slug: String,
    },
    /// Alias for archive: remove the workspace locally, keep the GitHub repo.
    Remove {
        /// Project slug.
        slug: String,
    },
    /// Register an existing directory under the projects root as a project.
    Adopt {
        /// Directory to adopt (must be inside projects_root).
        path: PathBuf,
        /// Override the slug derived from the directory name.
        #[arg(long)]
        slug: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum NotifyAction {
    /// Send a test message through every configured channel.
    Test,
    /// Show which channels are configured (never prints secrets).
    Status,
}

#[derive(Debug, Subcommand)]
pub enum CheckAction {
    /// Add a check (or refresh the open one with the same title).
    Add {
        /// What a human must verify.
        title: String,

        /// Project slug (defaults to the only project in the plan).
        #[arg(long)]
        project: Option<String>,

        /// When to check: +2h, 30m, 3d, 1w, now or an RFC3339 timestamp.
        #[arg(long)]
        due: Option<String>,

        /// vps, api, deploy, manual, data, security or other.
        #[arg(long, default_value = "manual")]
        kind: String,

        /// low, normal, high or critical.
        #[arg(long, default_value = "normal")]
        priority: String,

        /// Longer explanation shown under the check.
        #[arg(long, default_value = "")]
        detail: String,

        /// Recurrence: 1d, 12h, 30m (done reschedules instead of closing).
        #[arg(long)]
        every: Option<String>,

        /// Read-only VPS probe: `systemctl is-active <unit>`, `disk_free > 5G`,
        /// or `http 200 <url>`.
        #[arg(long)]
        probe: Option<String>,
    },

    /// Mark a check as done.
    Done {
        /// Check id (shown by `mox check`).
        id: String,

        /// Optional note about the outcome.
        #[arg(long, default_value = "")]
        note: String,
    },

    /// Reschedule a check (e.g. `mox check later a1b2c3d4 2h`).
    Later {
        /// Check id.
        id: String,
        /// New due date: +2h, 30m, 3d, 1w, now or RFC3339.
        when: String,
    },

    /// Import every project's CHECKS.md into the queue.
    Sync,

    /// Send a reminder digest of due checks through the configured channels.
    Notify,
    /// Remove a check permanently.
    Remove {
        /// Check id.
        id: String,
    },
    /// Reopen a completed check.
    Reopen {
        /// Check id.
        id: String,
    },
    /// Export the queue as markdown or json.
    Export {
        /// md or json.
        #[arg(long, default_value = "md", value_parser = ["md", "json"])]
        format: String,
    },
    /// Send due-check reminders periodically (Ctrl-C to quit).
    Daemon {
        /// Seconds between checks.
        #[arg(long, default_value_t = 300)]
        interval: u64,
    },
    /// Run the read-only VPS probes attached to checks and mark failures due.
    Probe {
        /// Only this project slug.
        #[arg(long)]
        slug: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum VpsCommand {
    /// Read-only capacity and reachability check.
    Check,
    /// Recommend where a workload of this size should run.
    Suggest {
        /// light, medium, heavy or gpu.
        #[arg(long, default_value = "medium")]
        compute: String,
    },
    /// Grant one project permission to deploy to the VPS (one-time human action).
    Allow {
        /// Project slug.
        slug: String,
        /// Why it is allowed / what it may do.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Revoke a VPS deployment approval.
    Revoke {
        /// Project slug.
        slug: String,
    },
    /// List projects with a VPS approval.
    Approvals,
    /// Check the remote prerequisites for VPS execution.
    Setup,
    /// Poll detached VPS runs, fetch finished work back, and update status.
    Sync {
        /// Only this project slug.
        #[arg(long)]
        slug: Option<String>,
        /// Keep polling (Ctrl-C to quit).
        #[arg(long)]
        watch: bool,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 60)]
        interval: u64,
    },
    /// List mox/agent processes on the VPS and flag untracked ghosts.
    Ps,
    /// Kill untracked mox ghosts (requires --apply --yes; never tracked runs).
    Cleanup {
        /// Actually send signals.
        #[arg(long)]
        apply: bool,
        /// Confirm the cleanup.
        #[arg(long)]
        yes: bool,
    },
    /// Print an OVHcloud provisioning checklist for a workload.
    Plan {
        /// light, medium, heavy or gpu.
        #[arg(long, default_value = "heavy")]
        compute: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_init_profile_and_completions() {
        let cli = Cli::try_parse_from(["mox", "init", "--profile", "paranoid"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Init {
                profile,
                force: false
            } if profile == "paranoid"
        ));
        let cli = Cli::try_parse_from(["mox", "completions", "bash"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions { ref shell } if shell == "bash"
        ));
    }

    #[test]
    fn parses_plan_flags() {
        let cli = Cli::try_parse_from([
            "mox",
            "--dry-run",
            "plan",
            "--ideas",
            "ideas.txt",
            "--answers",
            "answers.toml",
            "--yes",
        ])
        .unwrap();
        assert!(cli.dry_run);
        assert_eq!(cli.run_timeout, None);
        match cli.command {
            Command::Plan {
                ideas,
                answers,
                yes,
                deep,
                ..
            } => {
                assert_eq!(ideas.unwrap().to_str().unwrap(), "ideas.txt");
                assert_eq!(answers.unwrap().to_str().unwrap(), "answers.toml");
                assert!(yes);
                assert!(!deep);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_plan_approve_and_up_yes() {
        let cli = Cli::try_parse_from(["mox", "plan", "--approve"]).unwrap();
        assert!(matches!(cli.command, Command::Plan { approve: true, .. }));
        let cli = Cli::try_parse_from(["mox", "up", "--yes"]).unwrap();
        assert!(matches!(cli.command, Command::Up { yes: true, .. }));
    }

    #[test]
    fn parses_up_filters() {
        let cli = Cli::try_parse_from([
            "mox",
            "up",
            "--only",
            "alpha",
            "--only",
            "beta",
            "--max-parallel",
            "2",
            "--no-run",
        ])
        .unwrap();
        match cli.command {
            Command::Up {
                only,
                max_parallel,
                no_run,
                resume,
                retry_failed,
                target,
                owner,
                detach,
                ..
            } => {
                assert_eq!(only, vec!["alpha", "beta"]);
                assert_eq!(max_parallel, Some(2));
                assert!(no_run);
                assert!(!resume && !retry_failed);
                assert_eq!(target, "local");
                assert!(owner.is_none());
                assert!(!detach);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_clean_and_vps() {
        let cli = Cli::try_parse_from(["mox", "clean", "--apply", "--all"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Clean {
                apply: true,
                all: true,
                ..
            }
        ));
        let cli =
            Cli::try_parse_from(["mox", "vps", "allow", "demo", "--note", "deploy ok"]).unwrap();
        match cli.command {
            Command::Vps {
                action: VpsCommand::Allow { slug, note },
            } => {
                assert_eq!(slug, "demo");
                assert_eq!(note, "deploy ok");
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["mox", "vps", "suggest", "--compute", "heavy"]).unwrap();
        match cli.command {
            Command::Vps {
                action: VpsCommand::Suggest { compute },
            } => assert_eq!(compute, "heavy"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_global_run_timeout() {
        let cli = Cli::try_parse_from(["mox", "up", "--run-timeout", "1800"]).unwrap();
        assert_eq!(cli.run_timeout, Some(1800));
    }

    #[test]
    fn parses_check_commands() {
        let cli = Cli::try_parse_from([
            "mox",
            "check",
            "add",
            "Verify fetcher",
            "--project",
            "demo",
            "--due",
            "+2h",
            "--kind",
            "vps",
            "--priority",
            "high",
        ])
        .unwrap();
        match cli.command {
            Command::Check {
                action:
                    Some(CheckAction::Add {
                        title,
                        project,
                        due,
                        kind,
                        priority,
                        ..
                    }),
                ..
            } => {
                assert_eq!(title, "Verify fetcher");
                assert_eq!(project.as_deref(), Some("demo"));
                assert_eq!(due.as_deref(), Some("+2h"));
                assert_eq!(kind, "vps");
                assert_eq!(priority, "high");
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from(["mox", "check", "--json", "--all"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Check {
                action: None,
                json: true,
                all: true,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["mox", "check", "later", "abc", "2h"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Check {
                action: Some(CheckAction::Later { .. }),
                ..
            }
        ));
        let cli = Cli::try_parse_from(["mox", "attention"]).unwrap();
        assert!(matches!(cli.command, Command::Check { .. }));
    }

    #[test]
    fn parses_project_commands() {
        let cli = Cli::try_parse_from(["mox", "project", "archive", "demo"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Project {
                action: ProjectAction::Archive { .. }
            }
        ));
    }

    #[test]
    fn parses_project_adopt_and_sync_watch() {
        let cli = Cli::try_parse_from(["mox", "project", "adopt", "/tmp/x"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Project {
                action: ProjectAction::Adopt { .. }
            }
        ));
        let cli = Cli::try_parse_from(["mox", "vps", "sync", "--watch"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Vps {
                action: VpsCommand::Sync { watch: true, .. }
            }
        ));
    }

    #[test]
    fn parses_project_remove() {
        let cli = Cli::try_parse_from(["mox", "project", "remove", "demo"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Project {
                action: ProjectAction::Remove { .. }
            }
        ));
    }

    #[test]
    fn parses_recovery() {
        let cli = Cli::try_parse_from(["mox", "recovery", "--push", "--slug", "demo"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Recovery {
                push: true,
                slug: Some(ref s),
                ..
            } if s == "demo"
        ));
    }

    #[test]
    fn parses_events_and_log_retention() {
        let cli = Cli::try_parse_from(["mox", "events", "--tail", "5"]).unwrap();
        assert!(matches!(cli.command, Command::Events { tail: 5 }));
        let cli = Cli::try_parse_from(["mox", "clean", "--logs-older-than-days", "30"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Clean {
                logs_older_than_days: Some(30),
                ..
            }
        ));
    }

    #[test]
    fn parses_status_verbose_and_report() {
        let cli = Cli::try_parse_from(["mox", "status", "--verbose"]).unwrap();
        assert!(matches!(cli.command, Command::Status { verbose: true, .. }));
        let cli = Cli::try_parse_from(["mox", "report", "--out", "/tmp/r.md"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Report { out: Some(ref p) } if p == std::path::Path::new("/tmp/r.md")
        ));
    }

    #[test]
    fn parses_stats() {
        let cli = Cli::try_parse_from(["mox", "stats", "--json"]).unwrap();
        assert!(matches!(cli.command, Command::Stats { json: true, .. }));
    }

    #[test]
    fn parses_up_resume_and_retry() {
        let cli = Cli::try_parse_from(["mox", "up", "--resume"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Up {
                resume: true,
                retry_failed: false,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["mox", "up", "--retry-failed"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Up {
                retry_failed: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_up_owner() {
        let cli = Cli::try_parse_from(["mox", "up", "--owner", "my-org"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Up {
                owner: Some(ref o),
                ..
            } if o == "my-org"
        ));
    }

    #[test]
    fn parses_vps_ps_and_cleanup() {
        let cli = Cli::try_parse_from(["mox", "vps", "ps"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Vps {
                action: VpsCommand::Ps
            }
        ));
        let cli = Cli::try_parse_from(["mox", "vps", "cleanup", "--apply", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Vps {
                action: VpsCommand::Cleanup {
                    apply: true,
                    yes: true
                }
            }
        ));
    }

    #[test]
    fn parses_detach_and_vps_sync() {
        let cli = Cli::try_parse_from(["mox", "up", "--detach"]).unwrap();
        assert!(matches!(cli.command, Command::Up { detach: true, .. }));
        let cli = Cli::try_parse_from(["mox", "vps", "sync", "--slug", "demo"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Vps {
                action: VpsCommand::Sync { slug: Some(ref s), .. }
            } if s == "demo"
        ));
    }

    #[test]
    fn parses_up_target_and_vps_setup() {
        let cli = Cli::try_parse_from(["mox", "up", "--target", "vps"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Up { target, .. } if target == "vps"
        ));
        let cli = Cli::try_parse_from(["mox", "vps", "setup"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Vps {
                action: VpsCommand::Setup
            }
        ));
        let cli = Cli::try_parse_from(["mox", "vps", "plan", "--compute", "heavy"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Vps {
                action: VpsCommand::Plan { .. }
            }
        ));
    }

    #[test]
    fn parses_status_watch() {
        let cli = Cli::try_parse_from(["mox", "status", "--watch", "--interval", "5"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Status {
                watch: true,
                interval: 5,
                ..
            }
        ));
    }

    #[test]
    fn parses_milestone_commands() {
        let cli = Cli::try_parse_from(["mox", "milestone"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Milestone {
                action: None,
                file: None
            }
        ));
        let cli = Cli::try_parse_from([
            "mox",
            "milestone",
            "--file",
            "/tmp/M.json",
            "done",
            "m2",
            "--note",
            "shipped",
        ])
        .unwrap();
        match cli.command {
            Command::Milestone {
                file,
                action: Some(MilestoneAction::Done { id, note }),
            } => {
                assert_eq!(file.unwrap().to_str().unwrap(), "/tmp/M.json");
                assert_eq!(id, "m2");
                assert_eq!(note, "shipped");
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["mox", "milestone", "todo", "m1", "2", "--undo"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Milestone {
                action: Some(MilestoneAction::Todo {
                    index: 2,
                    undo: true,
                    ..
                }),
                ..
            }
        ));
        let cli =
            Cli::try_parse_from(["mox", "milestone", "add-todo", "m1", "write tests"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Milestone {
                action: Some(MilestoneAction::AddTodo { .. }),
                ..
            }
        ));
    }

    #[test]
    fn parses_check_extra_actions() {
        for (argv, ok) in [
            (vec!["mox", "check", "remove", "abc"], true),
            (vec!["mox", "check", "reopen", "abc"], true),
            (vec!["mox", "check", "export", "--format", "json"], true),
            (vec!["mox", "check", "daemon"], true),
            (vec!["mox", "check", "probe"], true),
        ] {
            assert!(Cli::try_parse_from(argv.clone()).is_ok(), "{argv:?}");
            assert!(ok);
        }
        let cli = Cli::try_parse_from(["mox", "check", "--top", "3"]).unwrap();
        assert!(matches!(cli.command, Command::Check { top: Some(3), .. }));
    }

    #[test]
    fn parses_bot() {
        let cli = Cli::try_parse_from(["mox", "bot", "--once"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Bot {
                once: true,
                allow_remote: false
            }
        ));
    }

    #[test]
    fn parses_balance_flags() {
        let cli = Cli::try_parse_from(["mox", "balance", "--json", "--notify"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Balance {
                json: true,
                notify: true
            }
        ));
    }

    #[test]
    fn parses_notify_commands() {
        let cli = Cli::try_parse_from(["mox", "notify", "test"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Notify {
                action: NotifyAction::Test
            }
        ));
        let cli = Cli::try_parse_from(["mox", "check", "notify"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Check {
                action: Some(CheckAction::Notify),
                ..
            }
        ));
    }

    #[test]
    fn parses_hidden_guard_command() {
        let cli =
            Cli::try_parse_from(["mox", "__guard", "git", "push", "--force", "origin"]).unwrap();
        match cli.command {
            Command::Guard { program, args } => {
                assert_eq!(program, "git");
                assert_eq!(args, vec!["push", "--force", "origin"]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_subcommand() {
        assert!(Cli::try_parse_from(["mox", "frobnicate"]).is_err());
    }

    #[test]
    fn global_config_flag_before_or_after_subcommand() {
        let before = Cli::try_parse_from(["mox", "--config", "/tmp/c.toml", "status"]).unwrap();
        let after = Cli::try_parse_from(["mox", "status", "--config", "/tmp/c.toml"]).unwrap();
        assert_eq!(
            before.config.unwrap().to_str().unwrap(),
            after.config.unwrap().to_str().unwrap()
        );
    }
}
