//! Host profile used to make planning estimates machine-aware.
//!
//! The planner (an LLM) is bad at guessing how fast *this* machine is. Passing
//! the CPU model, core count and RAM into the planning prompt anchors its
//! minute estimates; actual timings then calibrate the ETA as milestones
//! complete. We deliberately do not run a synthetic benchmark: the workload mix
//! (compile, network, data, tests) does not correlate well with a single CPU
//! score, and real milestone timings are a better signal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HostProfile {
    #[serde(default)]
    pub cpu_model: String,
    /// Logical cores (threads) available to schedule agents.
    #[serde(default)]
    pub cores: usize,
    /// Physical cores when the kernel exposes them, else 0/unknown.
    #[serde(default)]
    pub physical_cores: usize,
    /// Maximum observed CPU frequency in MHz (boost/turbo included).
    #[serde(default)]
    pub cpu_mhz: f64,
    #[serde(default)]
    pub mem_mb: u64,
    #[serde(default)]
    pub arch: String,
}

impl HostProfile {
    /// Read the local machine's basic characteristics. Never fails: missing
    /// files fall back to sensible placeholders.
    pub fn detect() -> Self {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        HostProfile {
            cpu_model: parse_cpu_model(&cpuinfo).unwrap_or_else(|| "unknown CPU".to_string()),
            cores,
            physical_cores: parse_physical_cores(&cpuinfo).unwrap_or(cores),
            cpu_mhz: parse_cpu_mhz(&cpuinfo).unwrap_or(0.0),
            mem_mb: crate::vps::read_mem_mb(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    pub fn describe(&self) -> String {
        let cores = if self.physical_cores > 0 && self.physical_cores != self.cores {
            format!(
                "{} physical / {} logical cores",
                self.physical_cores, self.cores
            )
        } else {
            format!("{} logical cores", self.cores)
        };
        let ghz = if self.cpu_mhz > 0.0 {
            format!(" · up to {:.1} GHz", self.cpu_mhz / 1000.0)
        } else {
            String::new()
        };
        format!(
            "{} · {}{} · {} RAM · {}",
            self.cpu_model,
            cores,
            ghz,
            crate::util::human_mb(self.mem_mb),
            self.arch
        )
    }

    /// Short guidance for the planning prompt.
    pub fn planning_guidance(&self) -> String {
        format!(
            "The target machine for these estimates is: {}. Assume one agent per \
             project, each pinned to its own logical cores and sharing the machine \
             with other agents. Estimate milestone durations \
             for this hardware, and say when a task is dominated by compilation, \
             network/API latency, data volume or pure CPU. When a milestone \
             involves a non-trivial algorithm, state the expected input scale \
             together with the complexity (for example `O(n log n), n≈10k rows`) \
             and derive `estimate_minutes` from it. Do not invent precision: \
             rough, honest ranges are better than optimistic single numbers.",
            self.describe()
        )
    }
}

/// Available memory in MiB from `/proc/meminfo` (`MemAvailable`), 0 if unknown.
pub fn mem_available_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("MemAvailable:")?;
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                Some(kb / 1024)
            })
        })
        .unwrap_or(0)
}

/// Parse the maximum `cpu MHz` value from `/proc/cpuinfo`.
pub fn parse_cpu_mhz(cpuinfo: &str) -> Option<f64> {
    cpuinfo
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.trim().eq_ignore_ascii_case("cpu MHz") {
                value.trim().parse::<f64>().ok()
            } else {
                None
            }
        })
        .fold(None, |max: Option<f64>, value| {
            Some(max.map_or(value, |m| m.max(value)))
        })
}

/// Count physical cores from unique `(physical id, core id)` pairs, falling
/// back to `cpu cores` × sockets.
pub fn parse_physical_cores(cpuinfo: &str) -> Option<usize> {
    use std::collections::BTreeSet;
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut sockets: BTreeSet<String> = BTreeSet::new();
    let mut cores_per_socket = 0usize;
    let mut current_physical: Option<String> = None;

    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "physical id" => {
                sockets.insert(value.to_string());
                current_physical = Some(value.to_string());
            }
            "core id" => {
                if let Some(physical) = &current_physical {
                    pairs.insert((physical.clone(), value.to_string()));
                }
            }
            "cpu cores" => {
                if let Ok(n) = value.parse::<usize>() {
                    cores_per_socket = cores_per_socket.max(n);
                }
            }
            _ => {}
        }
    }

    if !pairs.is_empty() {
        Some(pairs.len())
    } else if cores_per_socket > 0 {
        Some(cores_per_socket * sockets.len().max(1))
    } else {
        None
    }
}

/// Extract `model name` from `/proc/cpuinfo` (first occurrence).
pub fn parse_cpu_model(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("model name") {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_model_from_proc_fixture() {
        let fixture = "\
processor\t: 0
vendor_id\t: AuthenticAMD
model name\t: AMD Ryzen 5 3500U with Radeon Vega Mobile Gfx
cpu MHz\t\t: 2100.000

processor\t: 1
model name\t: AMD Ryzen 5 3500U with Radeon Vega Mobile Gfx
";
        assert_eq!(
            parse_cpu_model(fixture).unwrap(),
            "AMD Ryzen 5 3500U with Radeon Vega Mobile Gfx"
        );
        assert!(parse_cpu_model("no cpu info here").is_none());
    }

    #[test]
    fn parses_max_frequency_and_physical_cores() {
        let fixture = "\
processor\t: 0
model name\t: Test CPU
physical id\t: 0
cpu cores\t: 2
core id\t\t: 0
cpu MHz\t\t: 1200.000
processor\t: 1
physical id\t: 0
cpu cores\t: 2
core id\t\t: 1
cpu MHz\t\t: 3400.000
";
        assert_eq!(parse_cpu_mhz(fixture), Some(3400.0));
        assert_eq!(parse_physical_cores(fixture), Some(2));
        assert_eq!(parse_cpu_mhz("no mhz"), None);
        assert_eq!(parse_physical_cores("no cores"), None);
    }

    #[test]
    fn detect_returns_a_usable_profile() {
        let profile = HostProfile::detect();
        assert!(profile.cores >= 1);
        assert!(!profile.cpu_model.is_empty());
        assert!(!profile.arch.is_empty());
        let text = profile.describe();
        assert!(text.contains("cores"));
        assert!(text.contains("RAM"));
    }

    #[test]
    fn planning_guidance_mentions_hardware_and_complexity() {
        let profile = HostProfile {
            cpu_model: "Test CPU".into(),
            cores: 8,
            physical_cores: 4,
            cpu_mhz: 3200.0,
            mem_mb: 16_000,
            arch: "x86_64".into(),
        };
        let guidance = profile.planning_guidance();
        assert!(guidance.contains("Test CPU"));
        assert!(guidance.contains("8 logical cores"));
        assert!(guidance.contains("3.2 GHz"));
        assert!(guidance.contains("input scale"));
        assert!(guidance.contains("compilation"));
    }

    #[test]
    fn serde_roundtrip() {
        let profile = HostProfile {
            cpu_model: "X".into(),
            cores: 4,
            physical_cores: 2,
            cpu_mhz: 2500.0,
            mem_mb: 8000,
            arch: "x86_64".into(),
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert_eq!(serde_json::from_str::<HostProfile>(&json).unwrap(), profile);
    }
}
