//! Process execution behind a small trait so every workflow is testable with a
//! fake runner and no real GitHub/OpenCode calls.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A fully described process invocation. No shell is ever involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// 0 means "no timeout".
    pub timeout_secs: u64,
    /// When set, the child pid is written here as soon as it starts and removed
    /// when it exits. Used by `mox stop`.
    pub pid_file: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            timeout_secs: 0,
            pid_file: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn pid_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.pid_file = Some(path.into());
        self
    }

    /// Wrap this command in a prefix program, e.g. `taskset -c 0,1 <cmd>`.
    pub fn wrap(mut self, program: impl Into<String>, args: Vec<String>) -> Self {
        let inner_program = std::mem::replace(&mut self.program, program.into());
        let inner_args = self.args.clone();
        self.args = args;
        self.args.push(inner_program);
        self.args.extend(inner_args);
        self
    }

    pub fn display(&self) -> String {
        crate::util::render_command(&self.program, &self.args)
    }
}

/// Result of one process invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub pid: Option<u32>,
}

impl Output {
    pub fn success(&self) -> bool {
        !self.timed_out && !self.cancelled && self.code == Some(0)
    }

    pub fn ok() -> Self {
        Output {
            code: Some(0),
            ..Output::default()
        }
    }

    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Output {
            code: Some(code),
            stderr: stderr.into(),
            ..Output::default()
        }
    }

    /// Last non-empty lines of both streams, for error messages and logs.
    pub fn tail(&self, lines: usize) -> String {
        let mut all: Vec<&str> = self
            .stdout
            .lines()
            .chain(self.stderr.lines())
            .filter(|l| !l.trim().is_empty())
            .collect();
        if all.len() > lines {
            all = all.split_off(all.len() - lines);
        }
        all.join("\n")
    }

    pub fn summary(&self) -> String {
        if self.cancelled {
            "cancelled".to_string()
        } else if self.timed_out {
            "timed out".to_string()
        } else {
            match self.code {
                Some(c) => format!("exit {c}"),
                None => "terminated by signal".to_string(),
            }
        }
    }
}

/// Abstracts process execution.
pub trait CommandRunner: Send + Sync {
    fn run(&self, spec: &CommandSpec) -> Result<Output>;

    /// Commands recorded by fake runners; `None` for real execution.
    fn recorded_calls(&self) -> Option<Vec<CommandSpec>> {
        None
    }

    /// Run with stdout/stderr streamed to `log` (append). The default
    /// implementation is correct but buffers; `RealRunner` overrides it so long
    /// agent runs never fill a pipe.
    fn run_logged(&self, spec: &CommandSpec, log: &Path, banner: &str) -> Result<Output> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let header = format!("\n===== {banner} =====\n[{start}] $ {}\n", spec.display());
        append(log, &header)?;
        let output = self.run(spec)?;
        let tail = output.tail(400);
        append(
            log,
            &format!(
                "[end] {} pid={:?}\n{}\n",
                output.summary(),
                output.pid,
                tail
            ),
        )?;
        Ok(output)
    }
}

fn append(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        crate::util::ensure_dir_private(parent)?;
    }
    let mut file = crate::util::open_private_append(path)
        .with_context(|| format!("opening log {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// The production runner: spawns real processes, enforces timeouts, and streams
/// logged runs to disk.
pub struct RealRunner;

impl RealRunner {
    pub fn new() -> Self {
        RealRunner
    }
}

impl Default for RealRunner {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_and_wait(
    spec: &CommandSpec,
    stdout: std::process::Stdio,
    stderr: std::process::Stdio,
) -> Result<Output> {
    let mut command = std::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdout(stdout)
        .stderr(stderr)
        .stdin(std::process::Stdio::null());
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning `{}`", spec.display()))?;
    let pid = child.id();
    if let Some(pid_file) = &spec.pid_file {
        if let Some(parent) = pid_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = crate::util::write_private(pid_file, format!("{pid}\n"));
    }
    let deadline = if spec.timeout_secs == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(spec.timeout_secs))
    };

    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                let shutdown = crate::shutdown::requested();
                let deadline_hit = deadline.map(|d| Instant::now() >= d).unwrap_or(false);
                if shutdown || deadline_hit {
                    cancelled = shutdown;
                    timed_out = !shutdown;
                    terminate_group(&mut child, pid);
                    break child.wait()?;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    if let Some(pid_file) = &spec.pid_file {
        let _ = std::fs::remove_file(pid_file);
    }

    Ok(Output {
        code: status.code(),
        stdout: String::new(),
        stderr: String::new(),
        timed_out,
        cancelled,
        pid: Some(pid),
    })
}

/// Ask the whole process group to stop, then force it after a short grace.
fn terminate_group(child: &mut std::process::Child, pid: u32) {
    #[cfg(unix)]
    {
        let pgid = -(pid as i32);
        unsafe {
            libc::kill(pgid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return,
            }
        }
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (child, pid);
    }
}

impl CommandRunner for RealRunner {
    fn run(&self, spec: &CommandSpec) -> Result<Output> {
        use std::process::Stdio;
        let dir = std::env::temp_dir();
        let stamp = crate::util::timestamp_millis();
        let stdout_path = dir.join(format!("mox-{}-{stamp}.out", std::process::id()));
        let stderr_path = dir.join(format!("mox-{}-{stamp}.err", std::process::id()));
        let stdout = std::fs::File::create(&stdout_path)
            .with_context(|| format!("creating {}", stdout_path.display()))?;
        let stderr = std::fs::File::create(&stderr_path)
            .with_context(|| format!("creating {}", stderr_path.display()))?;
        let mut output = spawn_and_wait(spec, Stdio::from(stdout), Stdio::from(stderr))?;
        output.stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        output.stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
        Ok(output)
    }

    fn run_logged(&self, spec: &CommandSpec, log: &Path, banner: &str) -> Result<Output> {
        use std::process::Stdio;
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        append(
            log,
            &format!(
                "\n===== {banner} =====\n[{}] $ {}\n",
                crate::util::now_rfc3339(),
                spec.display()
            ),
        )?;
        // One append handle, cloned for both streams: child writes interleave
        // instead of truncating each other, and the file is never rewritten.
        let file = crate::util::open_private_append(log)
            .with_context(|| format!("opening log {}", log.display()))?;
        let stderr_file = file.try_clone()?;
        let output = spawn_and_wait(spec, Stdio::from(file), Stdio::from(stderr_file))?;
        append(
            log,
            &format!(
                "[{}] [end] {} pid={:?}\n",
                crate::util::now_rfc3339(),
                output.summary(),
                output.pid
            ),
        )?;
        Ok(output)
    }
}

/// A deterministic runner for tests. Records every call and answers with a
/// user-provided closure.
pub struct FakeRunner {
    calls: Mutex<Vec<CommandSpec>>,
    #[allow(clippy::type_complexity)]
    handler: Box<dyn Fn(&CommandSpec) -> Output + Send + Sync>,
}

impl FakeRunner {
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(&CommandSpec) -> Output + Send + Sync + 'static,
    {
        FakeRunner {
            calls: Mutex::new(Vec::new()),
            handler: Box::new(handler),
        }
    }

    /// Always succeeds with empty output.
    pub fn ok() -> Self {
        Self::new(|_| Output::ok())
    }

    /// Fails every command with the given stderr.
    pub fn failing(message: &str) -> Self {
        let message = message.to_string();
        Self::new(move |_| Output::failure(1, message.clone()))
    }

    pub fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// All programs invoked, in order (useful for asserting pipelines).
    pub fn programs(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.program.clone())
            .collect()
    }

    pub fn displays(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.display())
            .collect()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, spec: &CommandSpec) -> Result<Output> {
        self.calls.lock().unwrap().push(spec.clone());
        Ok((self.handler)(spec))
    }

    fn recorded_calls(&self) -> Option<Vec<CommandSpec>> {
        Some(self.calls())
    }
}

/// Run `spec` and turn a non-zero exit into an error carrying a short tail.
pub fn run_checked(runner: &dyn CommandRunner, spec: &CommandSpec, what: &str) -> Result<Output> {
    let output = runner.run(spec)?;
    if !output.success() {
        let tail = output.tail(12);
        bail!(
            "{what} failed ({}): {}\n{}",
            output.summary(),
            spec.display(),
            tail
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_commands_in_order() {
        let spec = CommandSpec::new("opencode")
            .arg("run")
            .arg("hello")
            .wrap("nice", vec!["-n".into(), "10".into()])
            .wrap("taskset", vec!["-c".into(), "0,1".into()]);
        assert_eq!(spec.program, "taskset");
        assert_eq!(
            spec.args,
            vec!["-c", "0,1", "nice", "-n", "10", "opencode", "run", "hello"]
        );
    }

    #[test]
    fn output_success_semantics() {
        assert!(Output::ok().success());
        assert!(!Output::failure(2, "boom").success());
        let timed = Output {
            timed_out: true,
            code: Some(0),
            ..Output::default()
        };
        assert!(!timed.success());
        assert_eq!(timed.summary(), "timed out");
        let cancelled = Output {
            cancelled: true,
            code: None,
            ..Output::default()
        };
        assert!(!cancelled.success());
        assert_eq!(cancelled.summary(), "cancelled");
    }

    #[test]
    fn tail_keeps_last_lines() {
        let out = Output {
            stdout: "1\n2\n3\n".into(),
            stderr: "err\n".into(),
            ..Output::default()
        };
        assert_eq!(out.tail(2), "3\nerr");
    }

    #[test]
    fn fake_runner_records_calls() {
        let runner = FakeRunner::ok();
        runner.run(&CommandSpec::new("echo").arg("hi")).unwrap();
        runner.run(&CommandSpec::new("true")).unwrap();
        assert_eq!(runner.call_count(), 2);
        assert_eq!(runner.programs(), vec!["echo", "true"]);
    }

    #[test]
    fn run_checked_reports_failure() {
        let runner = FakeRunner::failing("no auth");
        let err = run_checked(&runner, &CommandSpec::new("gh"), "gh call").unwrap_err();
        assert!(err.to_string().contains("gh call failed"));
        assert!(err.to_string().contains("no auth"));
    }

    #[test]
    fn real_runner_captures_stdout_and_code() {
        let runner = RealRunner::new();
        let out = runner
            .run(&CommandSpec::new("sh").args(["-c", "echo hello; exit 3"]))
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert!(out.stdout.contains("hello"));
        assert!(out.pid.is_some());
    }

    #[test]
    fn real_runner_enforces_timeout() {
        let runner = RealRunner::new();
        let started = Instant::now();
        let out = runner
            .run(&CommandSpec::new("sleep").arg("5").timeout(1))
            .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn run_logged_appends_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("nested").join("run.log");
        let runner = FakeRunner::ok();
        runner
            .run_logged(&CommandSpec::new("x"), &log, "banner")
            .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("banner"));
        assert!(text.contains("$ x"));
    }
}
