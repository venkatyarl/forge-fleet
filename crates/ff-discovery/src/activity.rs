use serde::{Deserialize, Serialize};
use std::{fs, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActivitySignals {
    pub user_idle_seconds: Option<u64>,
    pub cpu_pressure_percent: Option<f32>,
    pub recently_active: bool,
}

pub fn read_activity_signals() -> ActivitySignals {
    let idle = detect_user_idle_seconds();
    let cpu_pressure = detect_cpu_pressure_percent();

    let recently_active =
        idle.map(|v| v < 60).unwrap_or(false) || cpu_pressure.map(|v| v > 75.0).unwrap_or(false);

    ActivitySignals {
        user_idle_seconds: idle,
        cpu_pressure_percent: cpu_pressure,
        recently_active,
    }
}

fn detect_user_idle_seconds() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let output = run_cmd(
            "sh",
            &[
                "-lc",
                "ioreg -c IOHIDSystem | awk '/HIDIdleTime/ {print $NF; exit}'",
            ],
        )?;

        let nanos = output.trim().parse::<u64>().ok()?;
        return Some(nanos / 1_000_000_000);
    }

    None
}

fn detect_cpu_pressure_percent() -> Option<f32> {
    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(1) as f32;

    let load_1m = if cfg!(target_os = "linux") {
        fs::read_to_string("/proc/loadavg").ok().and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f32>().ok())
        })
    } else if cfg!(target_os = "macos") {
        run_cmd("sysctl", &["-n", "vm.loadavg"]).and_then(|s| {
            s.replace(['{', '}'], "")
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f32>().ok())
        })
    } else {
        None
    }?;

    Some((load_1m / cores * 100.0).clamp(0.0, 100.0))
}

fn run_cmd(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}
