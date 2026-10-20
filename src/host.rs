//! Host profile used to make planning estimates machine-aware.
//!
//! The planner (an LLM) is bad at guessing how fast *this* machine is. Passing
//! the CPU model, core count and RAM into the planning prompt anchors its
//! minute estimates; actual timings then calibrate the ETA as milestones
//! complete. We deliberately do not run a synthetic benchmark: the workload mix
//! (compile, network, data, tests) does not correlate well with a single CPU
//! score, and real milestone timings are a better signal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HostProfile {
    #[serde(default)]
    pub cpu_model: String,
    #[serde(default)]
    pub cores: usize,
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
        HostProfile {
            cpu_model: parse_cpu_model(&cpuinfo).unwrap_or_else(|| "unknown CPU".to_string()),
            cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            mem_mb: crate::vps::read_mem_mb(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} · {} cores · {} RAM · {}",
            self.cpu_model,
            self.cores,
            crate::util::human_mb(self.mem_mb),
            self.arch
        )
    }

    /// Short guidance for the planning prompt.
    pub fn planning_guidance(&self) -> String {
        format!(
            "The target machine for these estimates is: {}. Assume one agent per \
             project, sharing cores with other agents. Estimate milestone durations \
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
            mem_mb: 16_000,
            arch: "x86_64".into(),
        };
        let guidance = profile.planning_guidance();
        assert!(guidance.contains("Test CPU"));
        assert!(guidance.contains("8 cores"));
        assert!(guidance.contains("input scale"));
        assert!(guidance.contains("compilation"));
    }

    #[test]
    fn serde_roundtrip() {
        let profile = HostProfile {
            cpu_model: "X".into(),
            cores: 4,
            mem_mb: 8000,
            arch: "x86_64".into(),
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert_eq!(serde_json::from_str::<HostProfile>(&json).unwrap(), profile);
    }
}
