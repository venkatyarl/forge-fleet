//! V43: detect CX-7 / IB / RoCE fabric NICs + populate paired_with from
//! the fleet's known pair map. Called by heartbeat_v2 to annotate beats
//! with `Ip { kind: "cx7-fabric", paired_with: Some("adele"), link_speed_gbps: Some(200), ... }`.

use std::process::Command;

use crate::beat_v2::Ip;

/// Returns `Some(kind)` if the interface driver is `mlx5_core` (Mellanox
/// ConnectX family). Returns None for non-fabric NICs.
pub fn detect_fabric_kind(iface: &str) -> Option<String> {
    let out = Command::new("ethtool").arg("-i").arg(iface).output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    if !is_mlx5_driver(&txt) {
        return None;
    }
    Some("cx7-fabric".to_string())
}

/// True when `ethtool -i <iface>` output reports the `mlx5_core` driver
/// (Mellanox ConnectX family). Pure.
pub fn is_mlx5_driver(ethtool_i_output: &str) -> bool {
    ethtool_i_output
        .lines()
        .any(|l| l.starts_with("driver:") && l.contains("mlx5_core"))
}

/// Parse `ethtool <iface>` output to extract link speed in Gbps.
pub fn link_speed_gbps(iface: &str) -> Option<u32> {
    let out = Command::new("ethtool").arg(iface).output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    parse_ethtool_speed_gbps(&txt)
}

/// Extract link speed in Gbps from `ethtool <iface>` output (the `Speed:` line,
/// e.g. `Speed: 200000Mb/s` → `200`). `None` when the line is absent or
/// non-numeric (`Speed: Unknown!`). Pure.
pub fn parse_ethtool_speed_gbps(ethtool_output: &str) -> Option<u32> {
    for line in ethtool_output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Speed:") {
            let num: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            let mbps: u32 = num.parse().ok()?;
            return Some(mbps / 1000);
        }
    }
    None
}

/// Look up the paired-peer name for a fabric IP. Uses static maps for
/// known pairs; eventually should consult the `fabric_pairs` table for
/// operator-configured pairings.
///
/// Pairs (subnet → endpoints):
///   10.42.0.0/24  CX-7      sia ↔ adele
///   10.43.0.0/24  CX-7      rihanna ↔ beyonce
///   10.44.0.0/24  TB-3      taylor ↔ james
pub fn paired_peer_for(my_name: &str) -> Option<String> {
    let map: &[(&str, &str)] = &[
        ("sia", "adele"),
        ("adele", "sia"),
        ("rihanna", "beyonce"),
        ("beyonce", "rihanna"),
        ("taylor", "james"),
        ("james", "taylor"),
    ];
    for (a, b) in map {
        if my_name.eq_ignore_ascii_case(a) {
            return Some(b.to_string());
        }
    }
    None
}

/// Enrich an Ip entry with fabric metadata. Handles two fabric types:
/// - CX-7 (mlx5_core driver) → `cx7-fabric` kind, ethtool speed
/// - Thunderbolt (10.44.x or `medium=thunderbolt`) → `tb-fabric` kind,
///   already classified upstream by classify_iface; this just fills
///   paired_with based on the computer's name.
pub fn enrich_ip(ip: &mut Ip, my_computer_name: &str) {
    // CX-7: detected by driver. Overrides kind to cx7-fabric.
    if let Some(kind) = detect_fabric_kind(&ip.iface) {
        ip.kind = kind;
        ip.paired_with = paired_peer_for(my_computer_name);
        ip.link_speed_gbps = link_speed_gbps(&ip.iface);
        return;
    }
    // Thunderbolt: kind=tb-fabric was set upstream by classify_iface for
    // 10.44.x IPs. Fill paired_with from the static map.
    if ip.kind == "tb-fabric" {
        ip.paired_with = paired_peer_for(my_computer_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The three pure-parser tests below were authored by a fleet model (qwen36
    // on lily) via `ff offload`, hand-verified, then integrated — dogfooding the
    // fleet for test-gen. They pin the CX-7 fabric-detection parsing layer
    // (previously 0 tests) that annotates beats for the DGX TP=2 vLLM pairs.
    #[test]
    fn is_mlx5_driver_detects() {
        assert!(is_mlx5_driver("driver: mlx5_core\nversion: 5.x\n"));
        assert!(!is_mlx5_driver("driver: e1000e\nversion: 1.0\n"));
        assert!(!is_mlx5_driver(""));
    }

    #[test]
    fn parse_ethtool_speed_gbps_works() {
        assert_eq!(parse_ethtool_speed_gbps("\tSpeed: 200000Mb/s\n"), Some(200));
        assert_eq!(parse_ethtool_speed_gbps("Speed: Unknown!"), None);
        assert_eq!(parse_ethtool_speed_gbps("Duplex: Full"), None);
        assert_eq!(parse_ethtool_speed_gbps("Speed: 400000Mb/s"), Some(400));
    }

    #[test]
    fn paired_peer_for_pairs() {
        assert_eq!(paired_peer_for("sia").as_deref(), Some("adele"));
        assert_eq!(paired_peer_for("ADELE").as_deref(), Some("sia"));
        assert_eq!(paired_peer_for("taylor").as_deref(), Some("james"));
        assert_eq!(paired_peer_for("nobody").as_deref(), None);
    }
}
