//! Node hardware capability flags for HA decision tables.
//!
//! Provides a small, serializable snapshot of accelerator presence (NPU / iGPU)
//! that HA orchestration and decision-table routing can query without
//! depending on the full ff-core hardware profile.

use serde::{Deserialize, Serialize};

/// Detected node hardware capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Whether a Neural Processing Unit (discrete or integrated) is present.
    pub has_npu: bool,
    /// Whether an integrated GPU is present.
    pub has_igpu: bool,
    /// Total discrete GPU VRAM in GB summed across all GPUs. `None` when no
    /// discrete VRAM pool exists — no GPU at all, or a unified-memory GPU
    /// (Apple Silicon, NVIDIA GB10/DGX Spark reporting `N/A`, AMD APUs) whose
    /// pool is system RAM rather than dedicated VRAM.
    pub gpu_total_vram_gb: Option<f64>,
}

impl NodeInfo {
    /// Detect NPU/iGPU presence and total GPU VRAM on the current node.
    pub fn detect() -> Self {
        Self {
            has_npu: detect_npu(),
            has_igpu: detect_igpu(),
            gpu_total_vram_gb: detect_gpu_total_vram_gb(),
        }
    }
}

fn detect_npu() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Apple Silicon includes a Neural Engine.
        return is_apple_silicon();
    }

    #[cfg(target_os = "linux")]
    {
        if accel_device_present() {
            return true;
        }
        if let Some(out) = run_command("lspci", &[]) {
            return detect_npu_linux(&out);
        }
        false
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

fn detect_igpu() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Every Apple Silicon SoC includes an integrated GPU.
        return is_apple_silicon();
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(out) = run_command("lspci", &[]) {
            return detect_igpu_linux(&out);
        }
        false
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn is_apple_silicon() -> bool {
    run_command("sysctl", &["-n", "machdep.cpu.brand_string"])
        .map(|s| s.contains("Apple"))
        .unwrap_or(false)
}

/// Check for common Linux accelerator device nodes.
#[cfg(target_os = "linux")]
fn accel_device_present() -> bool {
    std::path::Path::new("/dev/accel").exists()
        || std::path::Path::new("/dev/npu0").exists()
        || std::fs::read_dir("/sys/class/accelerator")
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
}

/// Pure NPU detection from `lspci` output on Linux.
#[cfg(target_os = "linux")]
fn detect_npu_linux(lspci: &str) -> bool {
    for line in lspci.lines() {
        let lower = line.to_ascii_lowercase();
        // Intel AI Boost / Meteor Lake+ NPU, AMD XDNA, generic Neural Processing.
        if lower.contains("npu")
            || lower.contains("neural")
            || lower.contains("xdna")
            || lower.contains("intel(r) ai boost")
            || lower.contains("qualcomm")
        {
            return true;
        }
    }
    false
}

/// Pure iGPU detection from `lspci` output on Linux.
#[cfg(target_os = "linux")]
fn detect_igpu_linux(lspci: &str) -> bool {
    for line in lspci.lines() {
        let lower = line.to_ascii_lowercase();
        let is_display = lower.contains("vga")
            || lower.contains("3d")
            || lower.contains("display")
            || lower.contains("video");
        if !is_display {
            continue;
        }

        // Intel integrated graphics are almost always iGPUs.
        if lower.contains("intel") {
            return true;
        }

        // AMD APUs / integrated Radeon (avoid discrete RX/R9/R7 series).
        if lower.contains("amd") || lower.contains("radeon") {
            let discrete_markers = ["rx ", "r9 ", "r7 ", "rx5", "rx6", "rx7", "rx8", "rx9"];
            if !discrete_markers.iter().any(|m| lower.contains(m)) {
                return true;
            }
        }
    }
    false
}

/// Probe total discrete GPU VRAM in GB. Unified-memory GPUs report `None`:
/// there is no dedicated pool to size, and reporting system RAM as VRAM would
/// misinform placement decisions.
fn detect_gpu_total_vram_gb() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        // Apple Silicon GPUs share unified memory — no discrete VRAM pool.
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(out) = run_command(
            "nvidia-smi",
            &["--query-gpu=memory.total", "--format=csv,noheader,nounits"],
        ) {
            return parse_nvidia_smi_total_vram_gb(&out);
        }
        if let Some(out) = run_command("rocminfo", &[]) {
            return parse_rocminfo_total_vram_gb(&out);
        }
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Sum `memory.total` MiB values from `nvidia-smi` output, one line per GPU.
/// On unified-memory parts (GB10 / DGX Spark) nvidia-smi prints `N/A`; those
/// lines don't parse, so an all-unified host yields `None`.
#[cfg(target_os = "linux")]
fn parse_nvidia_smi_total_vram_gb(out: &str) -> Option<f64> {
    let total_mib: u64 = out
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .sum();
    (total_mib > 0).then(|| total_mib as f64 / 1024.0)
}

/// Sum the largest GLOBAL-segment pool of each GPU agent in `rocminfo` output.
/// Agents flagged `Memory Properties: APU` carve their pool from system RAM
/// (unified), so they contribute nothing.
#[cfg(target_os = "linux")]
fn parse_rocminfo_total_vram_gb(out: &str) -> Option<f64> {
    let mut total_kib: u64 = 0;
    let mut in_gpu = false;
    let mut is_apu = false;
    let mut agent_max_kib: u64 = 0;
    let mut in_global_pool = false;

    let mut flush = |in_gpu: bool, is_apu: bool, agent_max_kib: u64, total: &mut u64| {
        if in_gpu && !is_apu {
            *total += agent_max_kib;
        }
    };

    for line in out.lines() {
        let t = line.trim();
        if t.starts_with("Agent ") {
            flush(in_gpu, is_apu, agent_max_kib, &mut total_kib);
            in_gpu = false;
            is_apu = false;
            agent_max_kib = 0;
            in_global_pool = false;
        } else if let Some(v) = t.strip_prefix("Device Type:") {
            in_gpu = v.trim() == "GPU";
        } else if let Some(v) = t.strip_prefix("Memory Properties:") {
            is_apu = is_apu || v.contains("APU");
        } else if let Some(v) = t.strip_prefix("Segment:") {
            in_global_pool = v.contains("GLOBAL");
        } else if let Some(v) = t.strip_prefix("Size:") {
            // e.g. `Size: 16760832(0xFFC000) KB`
            if in_global_pool && v.trim_end().ends_with("KB") {
                let digits: String = v
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(kib) = digits.parse::<u64>() {
                    agent_max_kib = agent_max_kib.max(kib);
                }
            }
        }
    }
    flush(in_gpu, is_apu, agent_max_kib, &mut total_kib);

    (total_kib > 0).then(|| total_kib as f64 / (1024.0 * 1024.0))
}

fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_does_not_panic() {
        // Host-dependent; just ensure the probe returns a well-formed struct.
        let info = NodeInfo::detect();
        let _ = info.has_npu;
        let _ = info.has_igpu;
        let _ = info.gpu_total_vram_gb;
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn nvidia_smi_vram_sums_gpus_and_treats_na_as_unified() {
        assert_eq!(parse_nvidia_smi_total_vram_gb("24576\n"), Some(24.0));
        assert_eq!(parse_nvidia_smi_total_vram_gb("24576\n8192\n"), Some(32.0));
        // GB10 / DGX Spark unified memory: nvidia-smi reports N/A.
        assert_eq!(parse_nvidia_smi_total_vram_gb("[N/A]\n"), None);
        assert_eq!(parse_nvidia_smi_total_vram_gb(""), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn rocminfo_vram_counts_discrete_gpu_pools_only() {
        let discrete = "\
Agent 1
  Device Type:             CPU
  Pool Info:
    Pool 1
      Segment:                 GLOBAL; FLAGS: KERNARG, FINE GRAINED
      Size:                    131072000(0x7d00000) KB
Agent 2
  Device Type:             GPU
  Pool Info:
    Pool 1
      Segment:                 GLOBAL; FLAGS: COARSE GRAINED
      Size:                    16760832(0xffc000) KB
    Pool 2
      Segment:                 GROUP
      Size:                    64(0x40) KB
";
        let gb = parse_rocminfo_total_vram_gb(discrete).unwrap();
        assert!((gb - 15.984_375).abs() < 1e-9);

        // APU: the GPU agent's pool is unified system RAM, not VRAM.
        let apu = "\
Agent 2
  Device Type:             GPU
  Memory Properties:       APU
  Pool Info:
    Pool 1
      Segment:                 GLOBAL; FLAGS: COARSE GRAINED
      Size:                    8388608(0x800000) KB
";
        assert_eq!(parse_rocminfo_total_vram_gb(apu), None);
        assert_eq!(parse_rocminfo_total_vram_gb(""), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn detect_npu_linux_matches_known_strings() {
        assert!(detect_npu_linux(
            "00:00.0 Neural Processing Unit: Intel Corporation Intel(R) AI Boost"
        ));
        assert!(detect_npu_linux(
            "00:00.0 Processing accelerators: Advanced Micro Devices, Inc. [AMD/ATI] XDNA"
        ));
        assert!(!detect_npu_linux(
            "00:00.0 VGA compatible controller: NVIDIA Corporation GA104 [GeForce RTX 3070]"
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn detect_igpu_linux_matches_intel_and_amd_apu() {
        assert!(detect_igpu_linux(
            "00:02.0 VGA compatible controller: Intel Corporation Raptor Lake-P [Iris Xe Graphics]"
        ));
        assert!(detect_igpu_linux(
            "00:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Raphael"
        ));
        assert!(!detect_igpu_linux(
            "00:00.0 VGA compatible controller: NVIDIA Corporation GA104 [GeForce RTX 3070]"
        ));
        assert!(!detect_igpu_linux(
            "01:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Navi 33 [Radeon RX 7600M XT]"
        ));
    }
}
