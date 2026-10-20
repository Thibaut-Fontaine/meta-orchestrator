//! VPS awareness and execution-target decisions.
//!
//! All VPS interaction here is read-only (`nproc`, `free`, `df`, `hostname`,
//! `/proc/loadavg`). The orchestrator never starts, stops or edits services, and
//! never touches nginx, instabot or llmcheck.

use crate::exec::{CommandRunner, CommandSpec, Output};
use crate::model::ComputeNeed;

/// What we learned about the VPS. `reachable == false` carries `error`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VpsProbe {
    pub reachable: bool,
    pub hostname: String,
    pub cores: usize,
    pub mem_mb: u64,
    pub disk_free_mb: u64,
    pub load1: f64,
    pub error: Option<String>,
}

/// Capacity of the local workstation (or of the VPS, reused by tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCapacity {
    pub cores: usize,
    pub mem_mb: u64,
    pub disk_free_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionTarget {
    Local,
    Vps,
    /// Suggestion only: the owner must order/handle the remote plan manually.
    RemoteOvh {
        plan: String,
    },
}

impl ExecutionTarget {
    pub fn label(&self) -> String {
        match self {
            ExecutionTarget::Local => "local workstation".to_string(),
            ExecutionTarget::Vps => "existing VPS".to_string(),
            ExecutionTarget::RemoteOvh { plan } => format!("OVHcloud remote plan: {plan}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub target: ExecutionTarget,
    pub reasons: Vec<String>,
}

/// Build the read-only probe command for an SSH alias.
pub fn probe_command(alias: &str) -> CommandSpec {
    let remote = concat!(
        "printf 'CORES=%s\\n' \"$(nproc)\"; ",
        "printf 'HOST=%s\\n' \"$(hostname)\"; ",
        "free -m | awk '/^Mem:/{printf \"MEM_MB=%s\\n\", $2}'; ",
        "df -Pm / | awk 'NR==2{printf \"DISK_MB=%s\\n\", $4}'; ",
        "awk '{printf \"LOAD1=%s\\n\", $1}' /proc/loadavg"
    );
    CommandSpec::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=6"])
        .arg(alias)
        .arg(remote)
        .timeout(20)
}

/// Parse `KEY=value` probe output.
pub fn parse_probe(stdout: &str) -> Result<VpsProbe, String> {
    let mut cores = None;
    let mut mem_mb = None;
    let mut disk_free_mb = None;
    let mut hostname = String::new();
    let mut load1 = 0.0;
    for line in stdout.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "CORES" => cores = value.trim().parse::<usize>().ok(),
            "MEM_MB" => mem_mb = value.trim().parse::<u64>().ok(),
            "DISK_MB" => disk_free_mb = value.trim().parse::<u64>().ok(),
            "HOST" => hostname = value.trim().to_string(),
            "LOAD1" => load1 = value.trim().parse::<f64>().unwrap_or(0.0),
            _ => {}
        }
    }
    match (cores, mem_mb, disk_free_mb) {
        (Some(cores), Some(mem_mb), Some(disk_free_mb)) => Ok(VpsProbe {
            reachable: true,
            hostname,
            cores,
            mem_mb,
            disk_free_mb,
            load1,
            error: None,
        }),
        _ => Err("probe output is missing CORES, MEM_MB or DISK_MB".to_string()),
    }
}

/// Run the read-only probe. Never returns an error; failures are reported in the
/// result so the CLI can degrade gracefully.
pub fn probe(runner: &dyn CommandRunner, alias: &str) -> VpsProbe {
    let spec = probe_command(alias);
    match runner.run(&spec) {
        Ok(output) if output.success() => match parse_probe(&output.stdout) {
            Ok(probe) => probe,
            Err(err) => VpsProbe {
                error: Some(err),
                ..VpsProbe::default()
            },
        },
        Ok(output) => VpsProbe {
            error: Some(format!(
                "ssh probe failed ({}): {}",
                output.summary(),
                output.tail(4)
            )),
            ..VpsProbe::default()
        },
        Err(err) => VpsProbe {
            error: Some(format!("could not run ssh: {err:#}")),
            ..VpsProbe::default()
        },
    }
}

/// Decide where a project should run.
///
/// * `gpu` always needs a remote GPU plan.
/// * `light` stays local.
/// * `medium` stays local when the machine is comfortable, else uses the VPS.
/// * `heavy` prefers the VPS when it is strong enough, else suggests an OVH
///   remote plan.
pub fn recommend(
    compute: ComputeNeed,
    local: HostCapacity,
    vps: Option<&VpsProbe>,
) -> Recommendation {
    let vps_ok = vps.map(|v| v.reachable).unwrap_or(false);
    match compute {
        ComputeNeed::Light => Recommendation {
            target: ExecutionTarget::Local,
            reasons: vec!["light workload: no need to leave the workstation".to_string()],
        },
        ComputeNeed::Medium => {
            if local.cores >= 4 && local.disk_free_mb >= 2048 {
                Recommendation {
                    target: ExecutionTarget::Local,
                    reasons: vec![format!(
                        "medium workload fits locally ({} cores, {} free)",
                        local.cores,
                        crate::util::human_mb(local.disk_free_mb)
                    )],
                }
            } else if vps_ok {
                Recommendation {
                    target: ExecutionTarget::Vps,
                    reasons: vec![
                        "local machine is small or low on disk".to_string(),
                        "existing VPS has enough capacity for a medium workload".to_string(),
                    ],
                }
            } else {
                Recommendation {
                    target: ExecutionTarget::RemoteOvh {
                        plan: "c3-4 (4 vCores, 8 GB RAM)".to_string(),
                    },
                    reasons: vec![
                        "local machine is small and the VPS is unreachable".to_string(),
                        "a small OVHcloud plan is enough for a medium workload".to_string(),
                    ],
                }
            }
        }
        ComputeNeed::Heavy => {
            if let Some(vps) = vps.filter(|v| v.reachable) {
                if vps.cores >= 4 && vps.mem_mb >= 7000 && vps.disk_free_mb >= 10_000 {
                    Recommendation {
                        target: ExecutionTarget::Vps,
                        reasons: vec![format!(
                            "VPS has {} cores, {} RAM and {} free disk",
                            vps.cores,
                            crate::util::human_mb(vps.mem_mb),
                            crate::util::human_mb(vps.disk_free_mb)
                        )],
                    }
                } else {
                    Recommendation {
                        target: ExecutionTarget::RemoteOvh {
                            plan: "c3-8 (8 vCores, 32 GB RAM)".to_string(),
                        },
                        reasons: vec![format!(
                            "VPS is too small for a heavy workload ({} cores, {} RAM, {} free)",
                            vps.cores,
                            crate::util::human_mb(vps.mem_mb),
                            crate::util::human_mb(vps.disk_free_mb)
                        )],
                    }
                }
            } else {
                Recommendation {
                    target: ExecutionTarget::RemoteOvh {
                        plan: "c3-8 (8 vCores, 32 GB RAM)".to_string(),
                    },
                    reasons: vec![
                        "VPS is unreachable".to_string(),
                        "heavy workloads need dedicated remote CPU and disk".to_string(),
                    ],
                }
            }
        }
        ComputeNeed::Gpu => Recommendation {
            target: ExecutionTarget::RemoteOvh {
                plan: "GPU instance (t2-le-45 or larger)".to_string(),
            },
            reasons: vec![
                "GPU workloads cannot run on this workstation or the CPU-only VPS".to_string(),
                "order an OVHcloud GPU plan and point mox at it via a new ssh alias".to_string(),
            ],
        },
    }
}

/// Check the local machine's capacity.
pub fn local_capacity() -> HostCapacity {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    HostCapacity {
        cores,
        mem_mb: read_mem_mb(),
        disk_free_mb: 0, // filled by the caller through DiskProbe
    }
}

pub(crate) fn read_mem_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("MemTotal:")?;
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                Some(kb / 1024)
            })
        })
        .unwrap_or(0)
}

/// Human summary used by `mox doctor` and `mox vps check`.
pub fn describe(probe: &VpsProbe) -> String {
    if !probe.reachable {
        return format!(
            "unreachable ({})",
            probe.error.as_deref().unwrap_or("no details")
        );
    }
    format!(
        "{}: {} cores, {} RAM, {} free disk, load {:.2}",
        if probe.hostname.is_empty() {
            "vps"
        } else {
            &probe.hostname
        },
        probe.cores,
        crate::util::human_mb(probe.mem_mb),
        crate::util::human_mb(probe.disk_free_mb),
        probe.load1
    )
}

/// Format a probe for display. Kept separate from execution for tests.
pub fn render_probe(alias: &str, probe: &VpsProbe) -> String {
    let mut out = format!("VPS `{alias}`: {}\n", describe(probe));
    if probe.reachable {
        out.push_str(&format!(
            "  cores: {}\n  RAM: {}\n  disk free on /: {}\n  load (1m): {:.2}\n",
            probe.cores,
            crate::util::human_mb(probe.mem_mb),
            crate::util::human_mb(probe.disk_free_mb),
            probe.load1
        ));
    }
    out
}

#[allow(dead_code)]
fn _probe_from_output(output: &Output) -> VpsProbe {
    let _ = output;
    VpsProbe::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FakeRunner;

    const FIXTURE: &str = "CORES=4\nHOST=vps-ovh\nMEM_MB=3799\nDISK_MB=35000\nLOAD1=0.12\n";

    #[test]
    fn parses_probe_fixture() {
        let probe = parse_probe(FIXTURE).unwrap();
        assert!(probe.reachable);
        assert_eq!(probe.cores, 4);
        assert_eq!(probe.mem_mb, 3799);
        assert_eq!(probe.disk_free_mb, 35000);
        assert_eq!(probe.hostname, "vps-ovh");
        assert!((probe.load1 - 0.12).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_incomplete_probe() {
        assert!(parse_probe("CORES=4\nHOST=x\n").is_err());
    }

    #[test]
    fn probe_command_is_read_only() {
        let spec = probe_command("vps-ovh");
        assert_eq!(spec.program, "ssh");
        assert!(spec.args.contains(&"BatchMode=yes".to_string()));
        let remote = spec.args.last().unwrap();
        assert!(remote.contains("nproc"));
        assert!(remote.contains("df -Pm /"));
        for forbidden in ["systemctl", "rm ", "sudo", "nginx", "reboot"] {
            assert!(
                !remote.contains(forbidden),
                "probe must be read-only: {forbidden}"
            );
        }
    }

    #[test]
    fn probe_reports_unreachable() {
        let runner = FakeRunner::new(|_| Output::failure(255, "ssh: connect refused"));
        let probe = probe(&runner, "vps-ovh");
        assert!(!probe.reachable);
        assert!(probe.error.unwrap().contains("ssh probe failed"));
    }

    #[test]
    fn light_stays_local() {
        let rec = recommend(ComputeNeed::Light, capacity(8, 8000, 9000), None);
        assert_eq!(rec.target, ExecutionTarget::Local);
    }

    #[test]
    fn medium_uses_local_when_comfortable() {
        let rec = recommend(ComputeNeed::Medium, capacity(8, 8000, 9000), None);
        assert_eq!(rec.target, ExecutionTarget::Local);
    }

    #[test]
    fn medium_uses_vps_when_local_is_small() {
        let vps = parse_probe(FIXTURE).unwrap();
        let rec = recommend(ComputeNeed::Medium, capacity(2, 4000, 900), Some(&vps));
        assert_eq!(rec.target, ExecutionTarget::Vps);
    }

    #[test]
    fn medium_without_vps_suggests_small_plan() {
        let rec = recommend(ComputeNeed::Medium, capacity(2, 4000, 900), None);
        assert!(matches!(rec.target, ExecutionTarget::RemoteOvh { .. }));
    }

    #[test]
    fn heavy_uses_strong_vps() {
        let vps =
            parse_probe("CORES=8\nHOST=vps\nMEM_MB=16000\nDISK_MB=40000\nLOAD1=0.1\n").unwrap();
        let rec = recommend(ComputeNeed::Heavy, capacity(8, 8000, 9000), Some(&vps));
        assert_eq!(rec.target, ExecutionTarget::Vps);
    }

    #[test]
    fn heavy_avoids_weak_vps() {
        let vps = parse_probe(FIXTURE).unwrap();
        let rec = recommend(ComputeNeed::Heavy, capacity(8, 8000, 9000), Some(&vps));
        match rec.target {
            ExecutionTarget::RemoteOvh { plan } => assert!(plan.contains("c3-8")),
            other => panic!("expected remote plan, got {other:?}"),
        }
    }

    #[test]
    fn gpu_always_remote() {
        let rec = recommend(ComputeNeed::Gpu, capacity(64, 64000, 999999), None);
        assert!(matches!(rec.target, ExecutionTarget::RemoteOvh { .. }));
        assert!(rec.reasons.iter().any(|r| r.contains("GPU")));
    }

    fn capacity(cores: usize, mem_mb: u64, disk_free_mb: u64) -> HostCapacity {
        HostCapacity {
            cores,
            mem_mb,
            disk_free_mb,
        }
    }
}
