//! DeepSeek account balance watcher.
//!
//! The key is read from the OpenCode provider auth file (or
//! `DEEPSEEK_API_KEY`) and is only ever written to a 0600 curl config file.
//! Threshold crossings are persisted so a 30-minute timer can notify once per
//! threshold instead of every run.

use crate::config::Config;
use crate::exec::{CommandRunner, CommandSpec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const THRESHOLDS: [f64; 6] = [10.0, 5.0, 3.0, 2.0, 1.0, 0.5];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Balance {
    pub currency: String,
    pub total: f64,
    pub granted: f64,
    pub topped_up: f64,
    pub available: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BalanceState {
    #[serde(default)]
    pub checked_at: String,
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub last_balance: Option<f64>,
    #[serde(default)]
    pub notified: Vec<f64>,
}

impl BalanceState {
    pub fn path(cfg: &Config) -> PathBuf {
        cfg.state_dir_path().join("deepseek-balance.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading balance state {}", path.display()))?;
        let state = serde_json::from_str(&text)
            .with_context(|| format!("parsing balance state {}", path.display()))?;
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            crate::util::ensure_dir_private(parent)?;
        }
        let text = serde_json::to_string_pretty(self)? + "\n";
        crate::util::write_private(path, text)?;
        Ok(())
    }
}

/// Parse the documented `/user/balance` response.
pub fn parse_balance(raw: &str) -> Result<Balance> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("parsing the DeepSeek balance response")?;
    let available = value
        .get("is_available")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let infos = value
        .get("balance_infos")
        .and_then(|v| v.as_array())
        .context("DeepSeek response has no balance_infos array")?;
    if infos.is_empty() {
        bail!("DeepSeek response contains no balance entries");
    }
    let chosen = infos
        .iter()
        .find(|entry| entry.get("currency").and_then(|v| v.as_str()) == Some("USD"))
        .or_else(|| infos.first())
        .expect("non-empty balance_infos");
    let number = |key: &str| -> f64 {
        chosen
            .get(key)
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|text| text.parse::<f64>().ok())
                    .or_else(|| value.as_f64())
            })
            .unwrap_or(0.0)
    };
    Ok(Balance {
        currency: chosen
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("USD")
            .to_string(),
        total: number("total_balance"),
        granted: number("granted_balance"),
        topped_up: number("topped_up_balance"),
        available,
    })
}

/// Read the DeepSeek key without ever logging it.
pub fn api_key() -> Result<String> {
    if let Ok(value) = std::env::var("DEEPSEEK_API_KEY") {
        if !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
    }
    let home = crate::util::home_dir();
    let candidates = [
        home.join(".local/share/opencode/auth.json"),
        home.join(".config/opencode/auth.json"),
    ];
    for path in candidates {
        if let Some(key) = read_key_from_auth(&path)? {
            return Ok(key);
        }
    }
    bail!(
        "no DeepSeek key found; set DEEPSEEK_API_KEY or add the deepseek provider in OpenCode auth"
    )
}

fn read_key_from_auth(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let key = value
        .get("deepseek")
        .and_then(|provider| provider.get("key"))
        .and_then(|key| key.as_str())
        .unwrap_or_default()
        .trim();
    Ok((!key.is_empty()).then(|| key.to_string()))
}

/// Fetch the balance through `curl`, with the key only in a private config file.
pub fn fetch_balance(runner: Arc<dyn CommandRunner>, cfg: &Config, key: &str) -> Result<Balance> {
    let dir = cfg.state_dir_path().join("balance");
    crate::util::ensure_dir_private(&dir)?;
    let stamp = format!("{}-{}", std::process::id(), crate::util::timestamp_millis());
    let conf = dir.join(format!("curl-{stamp}.conf"));
    let text = format!(
        "url = \"https://api.deepseek.com/user/balance\"\n\
         header = \"Authorization: Bearer {}\"\n\
         header = \"Accept: application/json\"\n",
        curl_escape(key)
    );
    crate::util::write_private(&conf, text)?;
    let result = (|| -> Result<Balance> {
        let spec = CommandSpec::new("curl")
            .args(["-sS", "--fail", "--max-time", "20", "--config"])
            .arg(conf.display().to_string())
            .timeout(30);
        let output = runner.run(&spec)?;
        if !output.success() {
            bail!("DeepSeek balance request failed: {}", output.tail(4));
        }
        parse_balance(&output.stdout)
    })();
    let _ = std::fs::remove_file(&conf);
    result
}

fn curl_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

/// Thresholds crossed by `balance` that were not notified yet. The list is
/// updated in place, and thresholds above a topped-up balance are released so
/// a future drop can notify again.
pub fn crossings(balance: f64, notified: &mut Vec<f64>) -> Vec<f64> {
    notified.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    notified.dedup();
    notified.retain(|threshold| balance <= *threshold);
    let mut crossed = Vec::new();
    for threshold in THRESHOLDS {
        if balance <= threshold && !notified.iter().any(|v| (*v - threshold).abs() < 1e-9) {
            crossed.push(threshold);
            notified.push(threshold);
        }
    }
    crossed
}

pub fn next_thresholds(balance: f64) -> Vec<f64> {
    THRESHOLDS
        .iter()
        .copied()
        .filter(|threshold| balance > *threshold)
        .collect()
}

pub fn format_money(value: f64) -> String {
    format!("${value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{FakeRunner, Output};

    #[test]
    fn parses_a_usd_balance() {
        let raw = r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"7.80","granted_balance":"0.00","topped_up_balance":"7.80"}]}"#;
        let balance = parse_balance(raw).unwrap();
        assert_eq!(balance.currency, "USD");
        assert_eq!(balance.total, 7.8);
        assert!(balance.available);
    }

    #[test]
    fn crossing_ladder_notifies_once_and_releases_on_top_up() {
        let mut notified = Vec::new();
        assert_eq!(crossings(12.0, &mut notified), Vec::<f64>::new());
        assert_eq!(crossings(9.2, &mut notified), vec![10.0]);
        assert_eq!(crossings(9.1, &mut notified), Vec::<f64>::new());
        assert_eq!(crossings(4.9, &mut notified), vec![5.0]);
        assert_eq!(crossings(2.9, &mut notified), vec![3.0]);
        assert_eq!(crossings(0.4, &mut notified), vec![2.0, 1.0, 0.5]);
        // A top-up above 10 releases every lower threshold for a future drop.
        assert_eq!(crossings(12.0, &mut notified), Vec::<f64>::new());
        assert!(notified.is_empty());
        assert_eq!(crossings(9.0, &mut notified), vec![10.0]);
    }

    #[test]
    fn reads_a_key_from_the_auth_file_without_printing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"deepseek":{"type":"api","key":"sk-test-secret"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_key_from_auth(&path).unwrap().as_deref(),
            Some("sk-test-secret")
        );
    }

    #[test]
    fn curl_argv_does_not_contain_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeRunner::new(|spec| {
            assert_eq!(spec.program, "curl");
            assert!(!spec.args.iter().any(|arg| arg.contains("sk-secret")));
            let conf = &spec.args[spec.args.len() - 1];
            let text = std::fs::read_to_string(conf).unwrap();
            assert!(text.contains("Bearer sk-secret"));
            Output {
                code: Some(0),
                stdout:
                    r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"4.50","granted_balance":"0","topped_up_balance":"4.50"}]}"#
                        .into(),
                ..Output::default()
            }
        }));
        let cfg = Config {
            state_dir: dir.path().display().to_string(),
            ..Config::default()
        };
        let balance = fetch_balance(runner, &cfg, "sk-secret").unwrap();
        assert_eq!(balance.total, 4.5);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("balance"))
            .unwrap()
            .flatten()
            .collect();
        assert!(leftovers.is_empty());
    }
}
