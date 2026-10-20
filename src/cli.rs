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
    },

    /// Track milestones, progress and ETA for the project in the current dir.
    Milestone {
        /// Board file (default: ./MILESTONES.json).
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,

        #[command(subcommand)]
        action: Option<MilestoneAction>,
    },

    /// Send or inspect notifications (Telegram, desktop, webhook).
    Notify {
        #[command(subcommand)]
        action: NotifyAction,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
                ..
            } => {
                assert_eq!(ideas.unwrap().to_str().unwrap(), "ideas.txt");
                assert_eq!(answers.unwrap().to_str().unwrap(), "answers.toml");
                assert!(yes);
            }
            other => panic!("unexpected {other:?}"),
        }
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
                ..
            } => {
                assert_eq!(only, vec!["alpha", "beta"]);
                assert_eq!(max_parallel, Some(2));
                assert!(no_run);
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
