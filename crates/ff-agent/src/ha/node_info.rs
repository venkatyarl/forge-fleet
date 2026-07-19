//! Node hardware capability flags for HA decision tables.
//!
//! Provides a small, serializable snapshot of accelerator presence (NPU / iGPU)
//! that HA orchestration and decision-table routing can query without
//! depending on the full ff-core hardware profile.

use serde::{Deserialize, Serialize};

/// Detected node hardware capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Whether a Neural Processing Unit (discrete or integrated) is present.
    pub has_npu: bool,
    /// Whether an integrated GPU is present.
    pub has_igpu: bool,
}

impl NodeInfo {
    /// Detect NPU/iGPU presence on the current node.
    pub fn detect() -> Self {
        Self {
            has_npu: detect_npu(),
            has_igpu: detect_igpu(),
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
