use anyhow::Result;
use clap::Parser;
use meta_orchestrator::app::App;
use meta_orchestrator::brain::OpenCodeBrain;
use meta_orchestrator::cli::Cli;
use meta_orchestrator::config::{default_config_path, Config};
use meta_orchestrator::exec::RealRunner;
use meta_orchestrator::guard;
use meta_orchestrator::milestones;
use meta_orchestrator::notify;
use std::io::IsTerminal;
use std::sync::Arc;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if let meta_orchestrator::cli::Command::Guard { program, args } = &cli.command {
        guard::dispatch(program, args);
    }
    if matches!(
        &cli.command,
        meta_orchestrator::cli::Command::Milestone { .. }
    ) {
        let meta_orchestrator::cli::Command::Milestone { file, action } = cli.command else {
            unreachable!("checked above");
        };
        return milestones::run(action, file);
    }
    let cfg_path = cli.config.clone().unwrap_or_else(default_config_path);
    let mut cfg = Config::load(Some(&cfg_path))?;
    if let Some(root) = &cli.projects_root {
        cfg.projects_root = root.display().to_string();
    }
    if let Some(timeout) = cli.run_timeout {
        if timeout == 0 {
            anyhow::bail!("--run-timeout must be greater than zero");
        }
        cfg.run_timeout_secs = timeout;
    }
    cfg.validate()?;

    let dry_run = cli.dry_run
        || std::env::var("MOX_DRY_RUN")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);

    let interactive = match &cli.command {
        meta_orchestrator::cli::Command::Plan {
            yes, interactive, ..
        } => *interactive || (!*yes && std::io::stdin().is_terminal()),
        _ => false,
    };

    let runner = Arc::new(RealRunner::new());
    let brain = Arc::new(OpenCodeBrain::new(runner.clone(), &cfg));
    let notifier = notify::from_config(&cfg, runner.clone());
    let mut app =
        App::new(cfg, cfg_path, runner, brain, dry_run, interactive).with_notifier(notifier);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    app.run(cli.command, &mut out)?;
    Ok(())
}
