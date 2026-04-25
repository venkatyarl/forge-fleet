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
    let is_mlx = txt
        .lines()
        .any(|l| l.starts_with("driver:") && l.contains("mlx5_core"));
    if !is_mlx {
        return None;
    }
    Some("cx7-fabric".to_string())
}

/// Parse `ethtool <iface>` output to extract link speed in Gbps.
pub fn link_speed_gbps(iface: &str) -> Option<u32> {
    let out = Command::new("ethtool").arg(iface).output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    for line in txt.lines() {
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
