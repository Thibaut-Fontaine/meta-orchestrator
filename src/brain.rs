//! The "brain": the model call that turns rough ideas into questions and specs.
//!
//! In production this shells out to `opencode run`; in tests it is replaced by a
//! scripted brain, so the whole planning workflow is deterministic.

use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec};
use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Anything that can answer a prompt with text.
pub trait Brain: Send + Sync {
    fn think(&self, prompt: &str, timeout_secs: u64) -> Result<String>;
}

/// Brain backed by a real OpenCode run in an isolated planning directory.
pub struct OpenCodeBrain {
    runner: Arc<dyn CommandRunner>,
    binary: String,
    model: String,
    auto_approve: bool,
    nice_bin: String,
    nice_level: i32,
    cwd: PathBuf,
}

impl OpenCodeBrain {
    pub fn new(runner: Arc<dyn CommandRunner>, cfg: &Config) -> Self {
        OpenCodeBrain {
            runner,
            binary: cfg.opencode_bin.clone(),
            model: cfg.model.clone(),
            auto_approve: cfg.auto_approve,
            nice_bin: cfg.nice_bin.clone(),
            nice_level: cfg.nice_level,
            cwd: cfg.state_dir_path().join("planning"),
        }
    }

    /// The exact command issued for a planning call (exposed for tests).
    pub fn build_spec(&self, prompt: &str, timeout_secs: u64) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.binary)
            .arg("run")
            .timeout(timeout_secs)
            .cwd(&self.cwd)
            .env("MOX_ROLE", "planner");
        if self.auto_approve {
            spec = spec.arg("--auto");
        }
        if !self.model.trim().is_empty() {
            spec = spec.arg("--model").arg(self.model.trim());
        }
        spec = spec.arg("--title").arg("mox: planning");
        spec = spec.arg(prompt);
        if !self.nice_bin.trim().is_empty() {
            spec = spec.wrap(
                self.nice_bin.clone(),
                vec!["-n".to_string(), self.nice_level.to_string()],
            );
        }
        spec
    }
}

impl Brain for OpenCodeBrain {
    fn think(&self, prompt: &str, timeout_secs: u64) -> Result<String> {
        std::fs::create_dir_all(&self.cwd)?;
        let spec = self.build_spec(prompt, timeout_secs);
        let output = self.runner.run(&spec)?;
        if output.timed_out {
            bail!(
                "planning timed out after {timeout_secs}s; try a smaller idea list or a faster model"
            );
        }
        if !output.success() {
            bail!(
                "planning call failed ({}): {}",
                output.summary(),
                output.tail(10)
            );
        }
        if output.stdout.trim().is_empty() {
            bail!(
                "planning call returned no output{}",
                if output.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(" (stderr: {})", output.tail(5))
                }
            );
        }
        Ok(output.stdout)
    }
}

/// Deterministic brain used in tests: returns queued replies in order and
/// records every prompt.
#[derive(Default)]
pub struct ScriptedBrain {
    replies: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedBrain {
    pub fn new(replies: impl IntoIterator<Item = String>) -> Self {
        ScriptedBrain {
            replies: Mutex::new(replies.into_iter().collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl Brain for ScriptedBrain {
    fn think(&self, prompt: &str, _timeout_secs: u64) -> Result<String> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "{\"projects\":[]}".to_string());
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{FakeRunner, Output};

    fn cfg() -> Config {
        Config {
            state_dir: "/tmp/mox-state".into(),
            nice_bin: "nice".into(),
            nice_level: 10,
            model: "test/model".into(),
            ..Config::default()
        }
    }

    #[test]
    fn opencode_brain_builds_expected_command() {
        let runner = Arc::new(FakeRunner::ok());
        let brain = OpenCodeBrain::new(runner, &cfg());
        let spec = brain.build_spec("hello", 42);
        assert_eq!(spec.program, "nice");
        assert_eq!(
            spec.args,
            vec![
                "-n",
                "10",
                "opencode",
                "run",
                "--auto",
                "--model",
                "test/model",
                "--title",
                "mox: planning",
                "hello"
            ]
        );
        assert_eq!(spec.timeout_secs, 42);
        assert_eq!(
            spec.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/mox-state/planning"))
        );
    }

    #[test]
    fn opencode_brain_returns_stdout() {
        let runner = Arc::new(FakeRunner::new(|_| Output {
            code: Some(0),
            stdout: "{\"projects\":[]}".into(),
            ..Output::default()
        }));
        let brain = OpenCodeBrain::new(runner, &cfg());
        let text = brain.think("p", 10).unwrap();
        assert!(text.contains("projects"));
    }

    #[test]
    fn opencode_brain_reports_failures() {
        let runner = Arc::new(FakeRunner::failing("model unavailable"));
        let brain = OpenCodeBrain::new(runner, &cfg());
        let err = brain.think("p", 10).unwrap_err().to_string();
        assert!(err.contains("planning call failed"));
        assert!(err.contains("model unavailable"));
    }

    #[test]
    fn opencode_brain_reports_timeout() {
        let runner = Arc::new(FakeRunner::new(|_| Output {
            code: Some(0),
            timed_out: true,
            ..Output::default()
        }));
        let brain = OpenCodeBrain::new(runner, &cfg());
        assert!(brain
            .think("p", 10)
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }

    #[test]
    fn scripted_brain_queues_replies() {
        let brain = ScriptedBrain::new(vec!["one".to_string(), "two".to_string()]);
        assert_eq!(brain.think("a", 1).unwrap(), "one");
        assert_eq!(brain.think("b", 1).unwrap(), "two");
        assert_eq!(brain.prompts(), vec!["a", "b"]);
    }
}
