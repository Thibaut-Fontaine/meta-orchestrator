//! Notifications: tell the human what finished, what failed, what is waiting.
//!
//! Channels are optional and disabled by default. Telegram and webhook calls go
//! through `curl` with a private config file so the bot token or a secret webhook
//! URL never appears on a command line (where `ps` could see it).
//!
//! A notification failure must never fail the command that produced it; the app
//! prints a warning instead.

use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec};
use anyhow::{bail, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Questions,
    Progress,
    Finished,
    Failed,
    Checks,
    Digest,
    Test,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Questions => "questions",
            EventKind::Progress => "progress",
            EventKind::Finished => "finished",
            EventKind::Failed => "failed",
            EventKind::Checks => "checks",
            EventKind::Digest => "digest",
            EventKind::Test => "test",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub title: String,
    pub body: String,
}

impl Event {
    pub fn new(kind: EventKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Event {
            kind,
            title: title.into(),
            body: body.into(),
        }
    }

    /// Plain text used by Telegram and desktop notifications.
    pub fn text(&self) -> String {
        if self.body.trim().is_empty() {
            format!("[mox] {}", self.title)
        } else {
            format!("[mox] {}\n\n{}", self.title, self.body)
        }
    }
}

pub trait Notifier: Send + Sync {
    fn send(&self, event: &Event) -> Result<()>;
    fn name(&self) -> &'static str;
    /// Human description of the active channels (never includes secrets).
    fn describe(&self) -> Vec<String> {
        vec![self.name().to_string()]
    }
}

/// Does nothing (all channels disabled).
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn send(&self, _event: &Event) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "none"
    }

    fn describe(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Records events; used by tests and useful for `mox notify test` diagnostics.
#[derive(Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<Event>>,
}

impl RecordingNotifier {
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

impl Notifier for RecordingNotifier {
    fn send(&self, event: &Event) -> Result<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

pub struct DesktopNotifier {
    runner: Arc<dyn CommandRunner>,
}

impl DesktopNotifier {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        DesktopNotifier { runner }
    }
}

impl Notifier for DesktopNotifier {
    fn send(&self, event: &Event) -> Result<()> {
        let spec = CommandSpec::new("notify-send")
            .args(["-a", "mox"])
            .arg(format!("mox: {}", event.title))
            .arg(event.body.clone())
            .timeout(10);
        let output = self.runner.run(&spec)?;
        if !output.success() {
            bail!("notify-send failed: {}", output.tail(3));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "desktop"
    }
}

/// Telegram bot channel. The token is read from an environment variable at send
/// time and only ever written to a 0600 curl config file.
pub struct TelegramNotifier {
    runner: Arc<dyn CommandRunner>,
    token_env: String,
    chat_id: String,
    state_dir: PathBuf,
}

impl TelegramNotifier {
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        token_env: impl Into<String>,
        chat_id: impl Into<String>,
        state_dir: PathBuf,
    ) -> Self {
        TelegramNotifier {
            runner,
            token_env: token_env.into(),
            chat_id: chat_id.into(),
            state_dir,
        }
    }

    fn token(&self) -> Result<String> {
        let token = std::env::var(&self.token_env).unwrap_or_default();
        if token.trim().is_empty() {
            bail!("telegram is enabled but ${} is not set", self.token_env);
        }
        Ok(token.trim().to_string())
    }
}

impl Notifier for TelegramNotifier {
    fn send(&self, event: &Event) -> Result<()> {
        let token = self.token()?;
        let dir = self.state_dir.join("notify");
        crate::util::ensure_dir_private(&dir)?;
        let stamp = format!("{}-{}", std::process::id(), crate::util::timestamp_millis());
        let body_path = dir.join(format!("telegram-{stamp}.txt"));
        let conf_path = dir.join(format!("telegram-{stamp}.conf"));
        let _cleanup = TempFiles(vec![body_path.clone(), conf_path.clone()]);

        crate::util::write_private(&body_path, truncate(event.text(), 3800))?;
        let conf = format!(
            "url = \"https://api.telegram.org/bot{token}/sendMessage\"\n\
             data-urlencode = \"chat_id={chat}\"\n\
             data-urlencode = \"text@{body}\"\n",
            token = curl_escape(&token),
            chat = curl_escape(&self.chat_id),
            body = curl_escape(&body_path.display().to_string()),
        );
        crate::util::write_private(&conf_path, conf)?;

        let spec = CommandSpec::new("curl")
            .args(["-sS", "--max-time", "15", "--config"])
            .arg(conf_path.display().to_string())
            .timeout(25);
        let output = self.runner.run(&spec)?;
        if !output.success() {
            bail!("telegram request failed: {}", output.tail(4));
        }
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).unwrap_or(serde_json::Value::Null);
        if value.get("ok") == Some(&serde_json::Value::Bool(true)) {
            return Ok(());
        }
        let description = value
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown Telegram error");
        bail!("telegram rejected the message: {description}");
    }

    fn name(&self) -> &'static str {
        "telegram"
    }
}

/// Generic JSON webhook (ntfy, Slack proxy, custom bot, ...).
pub struct WebhookNotifier {
    runner: Arc<dyn CommandRunner>,
    url: String,
    state_dir: PathBuf,
}

impl WebhookNotifier {
    pub fn new(runner: Arc<dyn CommandRunner>, url: impl Into<String>, state_dir: PathBuf) -> Self {
        WebhookNotifier {
            runner,
            url: url.into(),
            state_dir,
        }
    }
}

#[derive(Serialize)]
struct WebhookBody<'a> {
    source: &'static str,
    kind: &'a str,
    title: &'a str,
    body: &'a str,
}

impl Notifier for WebhookNotifier {
    fn send(&self, event: &Event) -> Result<()> {
        if self.url.trim().is_empty() {
            bail!("webhook notifications are enabled but webhook_url is empty");
        }
        let dir = self.state_dir.join("notify");
        crate::util::ensure_dir_private(&dir)?;
        let stamp = format!("{}-{}", std::process::id(), crate::util::timestamp_millis());
        let body_path = dir.join(format!("webhook-{stamp}.json"));
        let conf_path = dir.join(format!("webhook-{stamp}.conf"));
        let _cleanup = TempFiles(vec![body_path.clone(), conf_path.clone()]);

        let body = serde_json::to_string(&WebhookBody {
            source: "mox",
            kind: event.kind.as_str(),
            title: &event.title,
            body: &event.body,
        })?;
        crate::util::write_private(&body_path, body)?;
        let conf = format!(
            "url = \"{}\"\nheader = \"Content-Type: application/json\"\n",
            curl_escape(self.url.trim())
        );
        crate::util::write_private(&conf_path, conf)?;

        let spec = CommandSpec::new("curl")
            .args(["-sS", "--max-time", "15", "--config"])
            .arg(conf_path.display().to_string())
            .arg("--data-binary")
            .arg(format!("@{}", body_path.display()))
            .timeout(25);
        let output = self.runner.run(&spec)?;
        if !output.success() {
            bail!("webhook request failed: {}", output.tail(4));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "webhook"
    }
}

/// Fan-out to several channels. Every channel is attempted even if one fails.
pub struct MultiNotifier {
    channels: Vec<Arc<dyn Notifier>>,
}

impl MultiNotifier {
    pub fn new(channels: Vec<Arc<dyn Notifier>>) -> Self {
        MultiNotifier { channels }
    }
}

impl Notifier for MultiNotifier {
    fn send(&self, event: &Event) -> Result<()> {
        let mut errors = Vec::new();
        for channel in &self.channels {
            if let Err(err) = channel.send(event) {
                errors.push(format!("{}: {err:#}", channel.name()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("; "))
        }
    }

    fn name(&self) -> &'static str {
        "multiple"
    }

    fn describe(&self) -> Vec<String> {
        self.channels.iter().flat_map(|c| c.describe()).collect()
    }
}

/// Build the notifier from configuration. Disabled channels are skipped.
pub fn from_config(cfg: &Config, runner: Arc<dyn CommandRunner>) -> Arc<dyn Notifier> {
    let mut channels: Vec<Arc<dyn Notifier>> = Vec::new();
    if cfg.notify_desktop {
        channels.push(Arc::new(DesktopNotifier::new(runner.clone())));
    }
    if cfg.notify_telegram && !cfg.telegram_chat_id.trim().is_empty() {
        channels.push(Arc::new(TelegramNotifier::new(
            runner.clone(),
            cfg.telegram_token_env.clone(),
            cfg.telegram_chat_id.clone(),
            cfg.state_dir_path(),
        )));
    }
    if cfg.notify_webhook && !cfg.webhook_url.trim().is_empty() {
        channels.push(Arc::new(WebhookNotifier::new(
            runner.clone(),
            cfg.webhook_url.clone(),
            cfg.state_dir_path(),
        )));
    }
    if channels.is_empty() {
        Arc::new(NullNotifier)
    } else {
        Arc::new(MultiNotifier::new(channels))
    }
}

struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn truncate(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text
    } else {
        let mut out: String = text.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Escape a value for a curl config file (`"` and `\`, newlines are not valid
/// in a quoted value and are replaced).
fn curl_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

pub fn channel_names(notifier: &dyn Notifier) -> String {
    let names = notifier.describe();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// Convenience used by tests and by `mox notify test`.
pub fn test_event() -> Event {
    Event::new(
        EventKind::Test,
        "mox notifications are working",
        "This is a test message from the meta-orchestrator.",
    )
}

/// Path helper so tests can reason about the temp directory.
pub fn notify_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("notify")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{FakeRunner, Output};
    use std::sync::Mutex;

    fn event() -> Event {
        Event::new(
            EventKind::Finished,
            "2 done",
            "demo: completed in 42s\nother: failed",
        )
    }

    #[test]
    fn event_text_includes_title_and_body() {
        assert!(event().text().contains("[mox] 2 done"));
        assert!(event().text().contains("demo: completed"));
    }

    #[test]
    fn desktop_command_is_well_formed() {
        let runner = Arc::new(FakeRunner::new(|spec| {
            assert_eq!(spec.program, "notify-send");
            assert_eq!(spec.args[0], "-a");
            assert_eq!(spec.args[1], "mox");
            assert_eq!(spec.args[2], "mox: 2 done");
            assert_eq!(spec.args[3], "demo: completed in 42s\nother: failed");
            Output::ok()
        }));
        DesktopNotifier::new(runner).send(&event()).unwrap();
    }

    #[test]
    fn telegram_keeps_the_token_out_of_argv() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let seen_conf = Arc::new(Mutex::new(String::new()));
        let seen_body = Arc::new(Mutex::new(String::new()));
        let conf_for_test = seen_conf.clone();
        let body_for_test = seen_body.clone();
        let runner = Arc::new(FakeRunner::new(move |spec| {
            assert_eq!(spec.program, "curl");
            let conf = &spec.args[spec.args.len() - 1];
            assert!(!spec.args.iter().any(|a| a.contains("secret-token")));
            let text = std::fs::read_to_string(conf).unwrap();
            *conf_for_test.lock().unwrap() = text.clone();
            // Find the body file referenced by the config.
            if let Some(line) = text.lines().find(|l| l.contains("text@")) {
                let path = line.split("text@").nth(1).unwrap().trim_end_matches('"');
                *body_for_test.lock().unwrap() = std::fs::read_to_string(path).unwrap();
            }
            Output {
                code: Some(0),
                stdout: r#"{"ok":true}"#.into(),
                ..Output::default()
            }
        }));
        let env_name = format!("MOX_TEST_TG_TOKEN_{}", std::process::id());
        std::env::set_var(&env_name, "secret-token");
        let notifier = TelegramNotifier::new(runner, &env_name, "12345", state);
        notifier.send(&event()).unwrap();
        std::env::remove_var(&env_name);

        let conf = seen_conf.lock().unwrap().clone();
        assert!(conf.contains("api.telegram.org/botsecret-token/sendMessage"));
        assert!(conf.contains("chat_id=12345"));
        assert!(seen_body.lock().unwrap().contains("demo: completed"));
        // Temp files are gone after a successful (or failed) send.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("notify"))
            .unwrap()
            .flatten()
            .collect();
        assert!(leftovers.is_empty(), "temp files must be cleaned up");
    }

    #[test]
    fn telegram_reports_api_errors_without_leaking_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeRunner::new(|_| Output {
            code: Some(0),
            stdout: r#"{"ok":false,"description":"Unauthorized"}"#.into(),
            ..Output::default()
        }));
        let env_name = format!("MOX_TEST_TG_BAD_{}", std::process::id());
        std::env::set_var(&env_name, "secret-token");
        let notifier = TelegramNotifier::new(runner, &env_name, "1", dir.path().to_path_buf());
        let err = notifier.send(&event()).unwrap_err().to_string();
        std::env::remove_var(&env_name);
        assert!(err.contains("Unauthorized"));
        assert!(!err.contains("secret-token"));
    }

    #[test]
    fn telegram_requires_the_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeRunner::ok());
        let notifier = TelegramNotifier::new(
            runner,
            "MOX_TEST_MISSING_TOKEN_XYZ",
            "1",
            dir.path().to_path_buf(),
        );
        let err = notifier.send(&event()).unwrap_err().to_string();
        assert!(err.contains("MOX_TEST_MISSING_TOKEN_XYZ"));
    }

    #[test]
    fn webhook_keeps_url_in_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_for_test = seen.clone();
        let runner = Arc::new(FakeRunner::new(move |spec| {
            assert_eq!(spec.program, "curl");
            assert!(!spec.args.iter().any(|a| a.contains("secret.example")));
            let conf = spec.args[4].clone();
            let text = std::fs::read_to_string(conf).unwrap();
            *seen_for_test.lock().unwrap() = text;
            Output::ok()
        }));
        let notifier = WebhookNotifier::new(
            runner,
            "https://secret.example/hook",
            dir.path().to_path_buf(),
        );
        notifier.send(&event()).unwrap();
        assert!(seen.lock().unwrap().contains("https://secret.example/hook"));
    }

    #[test]
    fn multi_attempts_every_channel_and_reports_failures() {
        struct Failing;
        impl Notifier for Failing {
            fn send(&self, _: &Event) -> Result<()> {
                bail!("boom")
            }
            fn name(&self) -> &'static str {
                "failing"
            }
        }
        let recorder = Arc::new(RecordingNotifier::default());
        let multi = MultiNotifier::new(vec![recorder.clone(), Arc::new(Failing)]);
        let err = multi.send(&event()).unwrap_err().to_string();
        assert!(err.contains("failing: boom"));
        assert_eq!(recorder.events().len(), 1);
        assert_eq!(multi.describe(), vec!["recording", "failing"]);
    }

    #[test]
    fn disabled_channels_produce_null_notifier() {
        let cfg = Config::default();
        let notifier = from_config(&cfg, Arc::new(FakeRunner::ok()));
        assert_eq!(notifier.name(), "none");
        assert_eq!(channel_names(notifier.as_ref()), "none");
    }

    #[test]
    fn config_enables_expected_channels() {
        let cfg = Config {
            notify_desktop: true,
            notify_telegram: true,
            telegram_chat_id: "42".into(),
            notify_webhook: true,
            webhook_url: "https://example.invalid/hook".into(),
            ..Config::default()
        };
        let notifier = from_config(&cfg, Arc::new(FakeRunner::ok()));
        assert_eq!(
            channel_names(notifier.as_ref()),
            "desktop, telegram, webhook"
        );
    }

    #[test]
    fn escapes_curl_values() {
        assert_eq!(curl_escape("a\"b\\c\nd"), "a\\\"b\\\\c d");
    }

    #[test]
    fn truncates_long_messages() {
        let long = "x".repeat(5000);
        assert!(truncate(long, 100).chars().count() <= 101);
    }
}
