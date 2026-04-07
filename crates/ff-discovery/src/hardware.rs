use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuProfile {
    pub model: String,
    pub physical_cores: usize,
    pub logical_cores: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuType {
    Metal,
    Cuda,
    Rocm,
    CpuOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Unified,
    Discrete,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProfile {
    pub total_bytes: u64,
    pub total_gb: f64,
    pub memory_type: MemoryType,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterconnectType {
    Thunderbolt,
    ConnectX7,
    Ethernet,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub cpu: CpuProfile,
    pub gpu_type: GpuType,
    pub gpu_devices: Vec<String>,
    pub memory: MemoryProfile,
    pub interconnect: InterconnectType,
    pub detected_at: DateTime<Utc>,
}

pub fn detect_hardware_profile() -> HardwareProfile {
    let (gpu_type, gpu_devices) = detect_gpu_info();

    HardwareProfile {
        hostname: detect_hostname(),
        os: std::env::consts::OS.to_string(),
        os_version: detect_os_version(),
        arch: std::env::consts::ARCH.to_string(),
        cpu: detect_cpu_profile(),
        memory: detect_memory_profile(gpu_type),
        interconnect: detect_interconnect(),
        gpu_type,
        gpu_devices,
        detected_at: Utc::now(),
    }
}

fn detect_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| run_command("hostname", &[]))
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn detect_os_version() -> String {
    if cfg!(target_os = "macos") {
        return run_command("sw_vers", &["-productVersion"])
            .map(|v| format!("macOS {v}"))
            .unwrap_or_else(|| "macOS".to_string());
    }

    if cfg!(target_os = "linux") {
        if let Ok(content) = fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                    return value.trim_matches('"').to_string();
                }
            }
        }
        return "Linux".to_string();
    }

    std::env::consts::OS.to_string()
}

fn detect_cpu_profile() -> CpuProfile {
    if cfg!(target_os = "macos") {
        let model = run_command("sysctl", &["-n", "machdep.cpu.brand_string"])
            .or_else(|| run_command("sysctl", &["-n", "machdep.cpu.leaf7_features"]))
            .unwrap_or_else(|| "Apple Silicon".to_string());

        let physical_cores = run_command("sysctl", &["-n", "hw.physicalcpu"])
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);

        let logical_cores = run_command("sysctl", &["-n", "hw.logicalcpu"])
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(physical_cores.max(1));

        return CpuProfile {
            model,
            physical_cores,
            logical_cores,
        };
    }

    if cfg!(target_os = "linux") {
        let logical_cores = run_command("nproc", &[])
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);

        let mut model = "Unknown Linux CPU".to_string();
        if let Ok(content) = fs::read_to_string("/proc/cpuinfo")
            && let Some(line) = content.lines().find(|line| line.starts_with("model name"))
            && let Some(name) = line.split(':').nth(1)
        {
            model = name.trim().to_string();
        }

        let physical_cores = detect_linux_physical_cores().unwrap_or(logical_cores);

        return CpuProfile {
            model,
            physical_cores,
            logical_cores,
        };
    }

    let logical_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    CpuProfile {
        model: "Unknown CPU".to_string(),
        physical_cores: logical_cores,
        logical_cores,
    }
}

fn detect_linux_physical_cores() -> Option<usize> {
    let lscpu = run_command("lscpu", &[])?;
    let mut cores_per_socket = None;
    let mut sockets = None;

    for line in lscpu.lines() {
        if line.starts_with("Core(s) per socket:") {
            cores_per_socket = line
                .split(':')
                .nth(1)
                .and_then(|s| s.trim().parse::<usize>().ok());
        }

        if line.starts_with("Socket(s):") {
            sockets = line
                .split(':')
                .nth(1)
                .and_then(|s| s.trim().parse::<usize>().ok());
        }
    }

    match (cores_per_socket, sockets) {
        (Some(c), Some(s)) => Some(c.saturating_mul(s)),
        _ => None,
    }
}

fn detect_gpu_info() -> (GpuType, Vec<String>) {
    if cfg!(target_os = "macos") {
        let devices = run_command("system_profiler", &["SPDisplaysDataType"])
            .map(|out| {
                out.lines()
                    .filter_map(|line| {
                        line.split_once(":")
                            .filter(|(k, _)| k.trim() == "Chipset Model")
                            .map(|(_, v)| v.trim().to_string())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        return (
            GpuType::Metal,
            if devices.is_empty() {
                vec!["Apple Integrated GPU".to_string()]
            } else {
                devices
            },
        );
    }

    if cfg!(target_os = "linux") {
        if let Some(out) = run_command("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"])
        {
            let devices = out
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();

            return (GpuType::Cuda, devices);
        }

        if let Some(out) = run_command("rocm-smi", &["--showproductname"]) {
            let devices = out
                .lines()
                .filter(|line| line.to_ascii_lowercase().contains("card series"))
                .map(|line| line.trim().to_string())
                .collect::<Vec<_>>();

            return (GpuType::Rocm, devices);
        }

        if let Some(out) = run_command("rocminfo", &[]) {
            let devices = out
                .lines()
                .filter_map(|line| {
                    line.split_once(':')
                        .filter(|(k, _)| k.trim() == "Marketing Name")
                        .map(|(_, v)| v.trim().to_string())
                })
                .collect::<Vec<_>>();

            if !devices.is_empty() {
                return (GpuType::Rocm, devices);
            }
        }

        if let Some(out) = run_command("sh", &["-lc", "lspci | grep -Ei 'vga|3d|display'"]) {
            let lines = out
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();

            if lines
                .iter()
                .any(|line| line.to_ascii_lowercase().contains("nvidia"))
            {
                return (GpuType::Cuda, lines);
            }

            if lines.iter().any(|line| {
                let l = line.to_ascii_lowercase();
                l.contains("amd") || l.contains("radeon") || l.contains("ati")
            }) {
                return (GpuType::Rocm, lines);
            }
        }
    }

    (GpuType::CpuOnly, vec![])
}

fn detect_memory_profile(gpu_type: GpuType) -> MemoryProfile {
    let total_bytes = if cfg!(target_os = "macos") {
        run_command("sysctl", &["-n", "hw.memsize"])
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    } else if cfg!(target_os = "linux") {
        parse_linux_memtotal_bytes().unwrap_or(0)
    } else {
        0
    };

    let memory_type = if cfg!(target_os = "macos") {
        MemoryType::Unified
    } else if matches!(gpu_type, GpuType::Cuda | GpuType::Rocm | GpuType::CpuOnly) {
        MemoryType::Discrete
    } else {
        MemoryType::Unknown
    };

    MemoryProfile {
        total_bytes,
        total_gb: (total_bytes as f64) / 1024_f64 / 1024_f64 / 1024_f64,
        memory_type,
    }
}

fn parse_linux_memtotal_bytes() -> Option<u64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let kb = content
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())?;

    Some(kb.saturating_mul(1024))
}

fn detect_interconnect() -> InterconnectType {
    if cfg!(target_os = "macos") {
        if let Some(out) = run_command("networksetup", &["-listallhardwareports"])
            && out.contains("Thunderbolt Bridge")
        {
            return InterconnectType::Thunderbolt;
        }

        if let Some(out) = run_command("ifconfig", &["-l"])
            && out
                .split_whitespace()
                .any(|iface| iface.starts_with("en") || iface.starts_with("eth"))
        {
            return InterconnectType::Ethernet;
        }

        return InterconnectType::Unknown;
    }

    if cfg!(target_os = "linux") {
        if let Some(out) = run_command("sh", &["-lc", "lspci | grep -i 'ConnectX-7'"])
            && !out.trim().is_empty()
        {
            return InterconnectType::ConnectX7;
        }

        if let Some(out) = run_command("ip", &["-o", "link", "show"])
            && out.lines().any(|line| {
                line.contains(" en")
                    || line.contains(" eth")
                    || line.contains(": enp")
                    || line.contains(": eno")
            })
        {
            return InterconnectType::Ethernet;
        }

        return InterconnectType::Unknown;
    }

    InterconnectType::Unknown
}

fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}
