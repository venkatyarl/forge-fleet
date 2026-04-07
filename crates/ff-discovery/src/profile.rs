use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fs, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuProfile {
    pub model: String,
    pub logical_cores: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProfile {
    pub total_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuProfile {
    pub name: String,
    pub vendor: Option<String>,
    pub memory_mb: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cpu: CpuProfile,
    pub memory: MemoryProfile,
    pub gpus: Vec<GpuProfile>,
    pub detected_at: DateTime<Utc>,
}

pub fn detect_hardware_profile() -> HardwareProfile {
    HardwareProfile {
        hostname: detect_hostname(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu: CpuProfile {
            model: detect_cpu_model(),
            logical_cores: std::thread::available_parallelism()
                .map(|c| c.get())
                .unwrap_or(1),
        },
        memory: MemoryProfile {
            total_mb: detect_total_memory_mb(),
        },
        gpus: detect_gpus(),
        detected_at: Utc::now(),
    }
}

fn detect_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| run_cmd("hostname", &[]))
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn detect_cpu_model() -> String {
    if cfg!(target_os = "macos") {
        run_cmd("sysctl", &["-n", "machdep.cpu.brand_string"])
            .or_else(|| run_cmd("sysctl", &["-n", "machdep.cpu.brand_string"]))
            .unwrap_or_else(|| "Apple Silicon".to_string())
    } else if cfg!(target_os = "linux") {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find(|line| line.starts_with("model name"))
                    .and_then(|line| line.split(':').nth(1))
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "Unknown Linux CPU".to_string())
    } else {
        "Unknown CPU".to_string()
    }
}

fn detect_total_memory_mb() -> u64 {
    if cfg!(target_os = "macos") {
        run_cmd("sysctl", &["-n", "hw.memsize"])
            .and_then(|s| s.parse::<u64>().ok())
            .map(|bytes| bytes / 1024 / 1024)
            .unwrap_or(0)
    } else if cfg!(target_os = "linux") {
        fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find(|line| line.starts_with("MemTotal:"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb / 1024)
            })
            .unwrap_or(0)
    } else {
        0
    }
}

fn detect_gpus() -> Vec<GpuProfile> {
    if cfg!(target_os = "macos") {
        return vec![GpuProfile {
            name: "Apple Integrated GPU".to_string(),
            vendor: Some("Apple".to_string()),
            memory_mb: None,
        }];
    }

    if cfg!(target_os = "linux")
        && let Some(output) = run_cmd("sh", &["-lc", "lspci | grep -Ei 'vga|3d|display'"])
    {
        let gpus: Vec<GpuProfile> = output
            .lines()
            .map(|line| GpuProfile {
                name: line.trim().to_string(),
                vendor: None,
                memory_mb: None,
            })
            .collect();

        if !gpus.is_empty() {
            return gpus;
        }
    }

    vec![]
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
