//! `ff fabric pair <a> <b> --kind cx7` — record that computers A and B are
//! linked by a private fabric (CX-7 / InfiniBand / RoCE). Does NOT assign
//! IPs; that's still a manual nmcli step. Inserts a `fabric_pairs` row
//! with NULL IPs so the materializer can fill them once both daemons
//! start emitting cx7-fabric Ip entries with `paired_with`.

use anyhow::{bail, Context, Result};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub async fn handle_fabric_pair(
    pg: &PgPool,
    a: &str,
    b: &str,
    kind: &str,
) -> Result<()> {
    if a == b {
        bail!("cannot pair a computer with itself");
    }
    let (a_name, b_name) = if a < b { (a, b) } else { (b, a) };
    let pair_name = format!("{}-{}", a_name, b_name);

    let row_a = sqlx::query("SELECT id FROM computers WHERE name = $1")
        .bind(a_name).fetch_optional(pg).await?
        .with_context(|| format!("computer '{}' not found", a_name))?;
    let row_b = sqlx::query("SELECT id FROM computers WHERE name = $1")
        .bind(b_name).fetch_optional(pg).await?
        .with_context(|| format!("computer '{}' not found", b_name))?;
    let a_id: Uuid = row_a.try_get("id")?;
    let b_id: Uuid = row_b.try_get("id")?;

    sqlx::query(
        "INSERT INTO fabric_pairs \
            (pair_name, fabric_kind, computer_a_id, computer_b_id, \
             a_iface, b_iface, a_ip, b_ip) \
         VALUES ($1, $2, $3, $4, '', '', '', '') \
         ON CONFLICT (pair_name) DO UPDATE SET fabric_kind = EXCLUDED.fabric_kind"
    )
    .bind(&pair_name).bind(kind).bind(a_id).bind(b_id)
    .execute(pg).await?;

    println!("Paired: {} (kind={})", pair_name, kind);
    println!("Next: configure IPs via nmcli on both hosts, then beats will auto-populate iface/ip.");
    Ok(())
}

/// `ff fabric benchmark <a> <b>` — run iperf3 across the fabric pair and
/// record measured throughput into `fabric_measurements`. Default: tests
/// both directions, single stream, 30s. Pass --reverse-only for B→A only.
pub async fn handle_fabric_benchmark(
    pg: &PgPool,
    a: &str,
    b: &str,
    duration: u32,
    streams: u32,
    reverse_only: bool,
) -> Result<()> {
    use std::process::Command as StdCommand;

    if a == b {
        bail!("cannot benchmark a computer against itself");
    }

    // 1. Find the fabric IP that A uses to reach B by intersecting both
    //    nodes' all_ips. We pick whichever subnet is shared (10.42.x for
    //    sia↔adele CX-7, 10.43.x for rihanna↔beyonce CX-7, 10.44.x for
    //    taylor↔james TB, etc.).
    let (a_fabric_ip, b_fabric_ip, fabric_kind, iface_a, iface_b, claimed_gbps) =
        resolve_fabric_endpoints(pg, a, b).await?;

    println!("Fabric link: {} ({}) ↔ {} ({}), kind={}{}",
        a, a_fabric_ip, b, b_fabric_ip, fabric_kind,
        claimed_gbps.map(|g| format!(", claimed={}Gbps", g)).unwrap_or_default());

    // 2. Look up SSH user for both nodes.
    let (a_ssh_user, _) = ff_agent::fleet_info::fetch_node_ip_user(a)
        .await
        .with_context(|| format!("could not resolve SSH for {}", a))?;
    let (b_ssh_user, _) = ff_agent::fleet_info::fetch_node_ip_user(b)
        .await
        .with_context(|| format!("could not resolve SSH for {}", b))?;
    let a_lan_ip = a_ssh_user.clone(); let _=a_lan_ip;
    // fetch_node_ip_user returns (ip, ssh_user) tuple — re-fetch for clarity
    let a_meta = ff_agent::fleet_info::fetch_node_by_name(a)
        .await.map_err(|e| anyhow::anyhow!(e))?
        .with_context(|| format!("computer '{}' not in fleet", a))?;
    let b_meta = ff_agent::fleet_info::fetch_node_by_name(b)
        .await.map_err(|e| anyhow::anyhow!(e))?
        .with_context(|| format!("computer '{}' not in fleet", b))?;

    let a_target = format!("{}@{}", a_meta.ssh_user, a_meta.ip);
    let b_target = format!("{}@{}", b_meta.ssh_user, b_meta.ip);

    // 3. Start iperf3 -s on b in background.
    println!("Starting iperf3 server on {}...", b);
    let _ = StdCommand::new("ssh")
        .args(["-o", "BatchMode=yes", &b_target,
               "pkill iperf3 2>/dev/null; iperf3 -s -D --logfile /tmp/iperf3.log"])
        .status();
    std::thread::sleep(std::time::Duration::from_millis(800));

    let mut measurements: Vec<(String, f64, Option<i32>)> = Vec::new();

    // 4. Forward direction A → B (unless reverse_only).
    if !reverse_only {
        println!("Running iperf3 client on {} → {} ({}s, {} streams)...",
                 a, b, duration, streams);
        let cmd = format!(
            "iperf3 -c {} -t {} -P {} -J",
            b_fabric_ip, duration, streams
        );
        let out = StdCommand::new("ssh")
            .args(["-o", "BatchMode=yes", &a_target, &cmd])
            .output()
            .context("ssh-iperf3 forward failed")?;
        let body = String::from_utf8_lossy(&out.stdout);
        let (gbps, retr) = parse_iperf3_json(&body);
        if gbps > 0.0 {
            println!("  {} → {}: {:.2} Gbps{}", a, b, gbps,
                     retr.map(|r| format!(" ({} retransmits)", r)).unwrap_or_default());
            measurements.push(("a_to_b".to_string(), gbps, retr));
        } else {
            println!("  {} → {}: failed (no parseable result)", a, b);
        }
    }

    // 5. Reverse direction B → A.
    println!("Running iperf3 client on {} → {} (reverse, {}s, {} streams)...",
             b, a, duration, streams);
    let cmd = format!(
        "iperf3 -c {} -t {} -P {} -R -J",
        b_fabric_ip, duration, streams
    );
    let out = StdCommand::new("ssh")
        .args(["-o", "BatchMode=yes", &a_target, &cmd])
        .output()
        .context("ssh-iperf3 reverse failed")?;
    let body = String::from_utf8_lossy(&out.stdout);
    let (gbps, retr) = parse_iperf3_json(&body);
    if gbps > 0.0 {
        println!("  {} → {}: {:.2} Gbps{}", b, a, gbps,
                 retr.map(|r| format!(" ({} retransmits)", r)).unwrap_or_default());
        measurements.push(("b_to_a".to_string(), gbps, retr));
    } else {
        println!("  {} → {}: failed", b, a);
    }

    // 6. Stop iperf3 server.
    let _ = StdCommand::new("ssh")
        .args(["-o", "BatchMode=yes", &b_target, "pkill iperf3 2>/dev/null"])
        .status();

    // 7. Record measurements.
    let measured_by = ff_agent::fleet_info::resolve_this_node_name().await;
    let iperf_version = StdCommand::new("iperf3").arg("--version").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(str::to_string));
    for (direction, gbps, retr) in &measurements {
        sqlx::query(
            "INSERT INTO fabric_measurements
                (node_a, node_b, iface_a, iface_b, fabric_kind, direction,
                 streams, duration_secs, measured_gbps, claimed_gbps,
                 retransmits, measured_by, iperf_version)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"
        )
        .bind(a).bind(b)
        .bind(&iface_a).bind(&iface_b)
        .bind(&fabric_kind).bind(direction)
        .bind(streams as i32).bind(duration as i32)
        .bind(*gbps).bind(claimed_gbps)
        .bind(retr.as_ref().copied()).bind(&measured_by)
        .bind(iperf_version.as_deref())
        .execute(pg).await?;
    }
    println!("Recorded {} measurement(s) into fabric_measurements.", measurements.len());
    Ok(())
}

/// `ff fabric measurements` — show recent throughput trend.
pub async fn handle_fabric_measurements(
    pg: &PgPool,
    a: Option<&str>,
    b: Option<&str>,
    limit: i64,
) -> Result<()> {
    let rows = if let (Some(an), Some(bn)) = (a, b) {
        sqlx::query(
            "SELECT measured_at, node_a, node_b, fabric_kind, direction,
                    measured_gbps, claimed_gbps, retransmits
               FROM fabric_measurements
              WHERE (node_a = $1 AND node_b = $2) OR (node_a = $2 AND node_b = $1)
              ORDER BY measured_at DESC
              LIMIT $3"
        ).bind(an).bind(bn).bind(limit).fetch_all(pg).await?
    } else {
        sqlx::query(
            "SELECT measured_at, node_a, node_b, fabric_kind, direction,
                    measured_gbps, claimed_gbps, retransmits
               FROM fabric_measurements
              ORDER BY measured_at DESC
              LIMIT $1"
        ).bind(limit).fetch_all(pg).await?
    };

    if rows.is_empty() {
        println!("(no measurements yet — run `ff fabric benchmark <a> <b>` to populate)");
        return Ok(());
    }

    println!("{:<22} {:<24} {:<14} {:<10} {:>8} {:>9} {:>7}",
             "MEASURED_AT", "PAIR", "FABRIC", "DIRECTION", "Gbps", "CLAIMED", "RETR");
    println!("{}", "-".repeat(100));
    for r in rows {
        let measured_at: chrono::DateTime<chrono::Utc> = r.try_get("measured_at")?;
        let na: String = r.try_get("node_a")?;
        let nb: String = r.try_get("node_b")?;
        let kind: String = r.try_get("fabric_kind")?;
        let dir: String = r.try_get("direction")?;
        let gbps: f64 = r.try_get("measured_gbps")?;
        let claimed: Option<i32> = r.try_get("claimed_gbps")?;
        let retr: Option<i32> = r.try_get("retransmits")?;
        println!("{:<22} {:<24} {:<14} {:<10} {:>8.2} {:>9} {:>7}",
            measured_at.format("%Y-%m-%d %H:%M:%S"),
            format!("{}↔{}", na, nb),
            kind, dir, gbps,
            claimed.map(|c| format!("{}Gbps", c)).unwrap_or_else(|| "-".into()),
            retr.map(|r| r.to_string()).unwrap_or_else(|| "-".into()));
    }
    Ok(())
}

/// Resolve the fabric IPs + iface names + kind for a benchmark run.
/// Reads each node's pulse beat (computers.all_ips) and finds the shared
/// fabric subnet.
async fn resolve_fabric_endpoints(
    pg: &PgPool,
    a: &str,
    b: &str,
) -> Result<(String, String, String, String, String, Option<i32>)> {
    use serde_json::Value;
    let a_ips_raw: Option<(Value,)> = sqlx::query_as(
        "SELECT all_ips FROM computers WHERE name = $1"
    ).bind(a).fetch_optional(pg).await?;
    let b_ips_raw: Option<(Value,)> = sqlx::query_as(
        "SELECT all_ips FROM computers WHERE name = $1"
    ).bind(b).fetch_optional(pg).await?;

    let a_ips = a_ips_raw.with_context(|| format!("computer '{}' not in DB", a))?.0;
    let b_ips = b_ips_raw.with_context(|| format!("computer '{}' not in DB", b))?.0;

    let parse = |v: &Value| -> Vec<(String, String, String, Option<u32>)> {
        v.as_array().map(|a| a.iter().filter_map(|item| {
            let ip = item.get("ip")?.as_str()?.to_string();
            let iface = item.get("iface")?.as_str()?.to_string();
            let kind = item.get("kind")?.as_str()?.to_string();
            let speed = item.get("link_speed_gbps").and_then(|x| x.as_u64().map(|n| n as u32));
            Some((ip, iface, kind, speed))
        }).collect()).unwrap_or_default()
    };
    let a_list = parse(&a_ips);
    let b_list = parse(&b_ips);

    // Match by shared /24 subnet on a fabric kind.
    for (aip, aif, akind, aspeed) in &a_list {
        if !akind.ends_with("-fabric") { continue; }
        let prefix: String = aip.rsplitn(2, '.').nth(1).unwrap_or("").to_string();
        if prefix.is_empty() { continue; }
        for (bip, bif, bkind, _) in &b_list {
            if bkind != akind { continue; }
            let bprefix: String = bip.rsplitn(2, '.').nth(1).unwrap_or("").to_string();
            if prefix == bprefix && aip != bip {
                return Ok((aip.clone(), bip.clone(), akind.clone(), aif.clone(), bif.clone(),
                           aspeed.map(|s| s as i32)));
            }
        }
    }
    bail!("no shared fabric subnet found between {} and {}; are both ends configured? \
           (run `ff openclaw status` and verify all_ips on each computer)", a, b)
}

/// Parse iperf3 -J JSON output. Returns (Gbps, retransmits).
fn parse_iperf3_json(body: &str) -> (f64, Option<i32>) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (0.0, None),
    };
    // sum_sent.bits_per_second is the canonical end-of-test result.
    let bps = v.get("end")
        .and_then(|e| e.get("sum_sent"))
        .and_then(|s| s.get("bits_per_second"))
        .and_then(|n| n.as_f64())
        .unwrap_or(0.0);
    let retr = v.get("end")
        .and_then(|e| e.get("sum_sent"))
        .and_then(|s| s.get("retransmits"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);
    (bps / 1e9, retr)
}
