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

/// Look up the paired-peer name for a fabric IP. Uses the DGX CX-7 pair map
/// from `reference_dgx_connectx7_pairs.md` as a fallback. Eventually should
/// consult the `fabric_pairs` table for operator-configured pairings.
pub fn paired_peer_for(my_name: &str) -> Option<String> {
    let map: &[(&str, &str)] = &[
        ("sia", "adele"),
        ("adele", "sia"),
        ("rihanna", "beyonce"),
        ("beyonce", "rihanna"),
    ];
    for (a, b) in map {
        if my_name.eq_ignore_ascii_case(a) {
            return Some(b.to_string());
        }
    }
    None
}

/// Enrich an Ip entry with fabric metadata if its interface is mlx5_core.
pub fn enrich_ip(ip: &mut Ip, my_computer_name: &str) {
    if let Some(kind) = detect_fabric_kind(&ip.iface) {
        ip.kind = kind;
        ip.paired_with = paired_peer_for(my_computer_name);
        ip.link_speed_gbps = link_speed_gbps(&ip.iface);
    }
}
