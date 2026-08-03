//! `ff fabric pair <a> <b> --kind cx7` — record that computers A and B are
//! linked by a private fabric (CX-7 / InfiniBand / RoCE). Does NOT assign
//! IPs; that's still a manual nmcli step. Inserts a `fabric_pairs` row
//! with NULL IPs so the materializer can fill them once both daemons
//! start emitting cx7-fabric Ip entries with `paired_with`.

use std::net::{IpAddr, Ipv4Addr};
use std::process::Command as StdCommand;

use anyhow::{Context, Result, bail};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const RECONCILE_CONFLICT_SQL: &str = "SELECT pair_name FROM fabric_pairs
      WHERE (pair_name = $1
             AND NOT ((computer_a_id = $2 AND computer_b_id = $4)
                   OR (computer_a_id = $4 AND computer_b_id = $2)))
         OR (endpoints_explicit
             AND pair_name <> $1
             AND (
               ((computer_a_id = $2 AND computer_b_id = $4)
                 OR (computer_a_id = $4 AND computer_b_id = $2))
               OR (computer_a_id = $2 AND a_iface = $3)
               OR (computer_b_id = $2 AND b_iface = $3)
               OR (computer_a_id = $4 AND a_iface = $5)
               OR (computer_b_id = $4 AND b_iface = $5)
               OR NULLIF(a_ip, '') = $6 OR NULLIF(b_ip, '') = $6
               OR NULLIF(a_ip, '') = $7 OR NULLIF(b_ip, '') = $7
               OR cidr = $8))
      LIMIT 1";

const RECONCILE_LEGACY_CLEANUP_SQL: &str = "DELETE FROM fabric_pairs
      WHERE NOT endpoints_explicit
        AND pair_name <> $1
        AND ((computer_a_id = $2 AND computer_b_id = $3)
          OR (computer_a_id = $3 AND computer_b_id = $2))";

const REMOVE_EXACT_LINK_SQL: &str = "DELETE FROM fabric_pairs
      WHERE fabric_kind=$1
        AND ((computer_a_id=$2 AND computer_b_id=$3
              AND a_iface=$4 AND a_ip=$5 AND b_iface=$6 AND b_ip=$7)
          OR (computer_a_id=$3 AND computer_b_id=$2
              AND a_iface=$6 AND a_ip=$7 AND b_iface=$4 AND b_ip=$5))";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FabricEndpoint {
    pub node: String,
    pub iface: String,
    pub ip: IpAddr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FabricLinkSpec {
    pub a: FabricEndpoint,
    pub b: FabricEndpoint,
    pub kind: String,
}

impl FabricLinkSpec {
    pub fn new(
        a: &str,
        a_iface: &str,
        a_ip: &str,
        b: &str,
        b_iface: &str,
        b_ip: &str,
        kind: &str,
    ) -> Result<Self> {
        let endpoint = |node: &str, iface: &str, ip: &str| -> Result<FabricEndpoint> {
            if node.trim().is_empty() || iface.trim().is_empty() {
                bail!("node and interface must be non-empty");
            }
            if !node
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                || !iface
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                bail!("node and interface contain unsupported characters");
            }
            Ok(FabricEndpoint {
                node: node.trim().to_ascii_lowercase(),
                iface: iface.trim().to_string(),
                ip: ip.parse().with_context(|| format!("invalid IP '{ip}'"))?,
            })
        };
        let mut a = endpoint(a, a_iface, a_ip)?;
        let mut b = endpoint(b, b_iface, b_ip)?;
        if a.node == b.node || a.ip == b.ip {
            bail!("fabric endpoints must be distinct");
        }
        let kind = kind.trim().to_ascii_lowercase();
        if kind.is_empty()
            || !kind
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("fabric kind must be non-empty and contain only safe characters");
        }
        if (&a.node, &a.iface, a.ip) > (&b.node, &b.iface, b.ip) {
            std::mem::swap(&mut a, &mut b);
        }
        Ok(Self { a, b, kind })
    }

    fn pair_name(&self) -> String {
        format!("{}-{}", self.a.node, self.b.node)
    }

    fn subnet_key(ip: IpAddr) -> Result<String> {
        match ip {
            IpAddr::V4(ip) => {
                let raw = u32::from(ip);
                if !matches!(raw & 3, 1 | 2) {
                    bail!("fabric endpoint {ip} is not a usable host address in its /30");
                }
                Ok(format!("{}/30", Ipv4Addr::from(raw & !3)))
            }
            IpAddr::V6(_) => bail!("fabric reconciliation currently requires IPv4 /30 links"),
        }
    }

    fn validate_link_subnet(&self) -> Result<String> {
        let a = Self::subnet_key(self.a.ip)?;
        let b = Self::subnet_key(self.b.ip)?;
        if a != b {
            bail!("endpoints are not in the same /30 subnet ({a} vs {b})");
        }
        Ok(a)
    }
}

pub async fn handle_fabric_reconcile(pg: &PgPool, spec: FabricLinkSpec, apply: bool) -> Result<()> {
    let subnet = spec.validate_link_subnet()?;
    let mut tx = pg.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ff-fabric-reconcile'))")
        .execute(&mut *tx)
        .await?;

    let a_id = computer_id(&mut tx, &spec.a.node).await?;
    let b_id = computer_id(&mut tx, &spec.b.node).await?;
    let pair_name = spec.pair_name();
    // Only operator-explicit rows have authority to veto endpoint/subnet reuse.
    // Legacy hints can contain crossed endpoints and stale subnets, so they do
    // not block a correct declaration. A canonical-name collision owned by an
    // unrelated node pair and a second explicit row for this pair both fail
    // closed instead of being overwritten or auto-deleted.
    let conflict = sqlx::query(RECONCILE_CONFLICT_SQL)
        .bind(&pair_name)
        .bind(a_id)
        .bind(&spec.a.iface)
        .bind(b_id)
        .bind(&spec.b.iface)
        .bind(spec.a.ip.to_string())
        .bind(spec.b.ip.to_string())
        .bind(&subnet)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(row) = conflict {
        let owner: String = row.try_get("pair_name")?;
        bail!("endpoint or subnet is already used by fabric link '{owner}'");
    }

    // Remove only non-authoritative legacy orientations for this exact
    // unordered node pair before the canonical upsert. Never delete an
    // operator-explicit link or a hint belonging to an unrelated pair.
    sqlx::query(RECONCILE_LEGACY_CLEANUP_SQL)
        .bind(&pair_name)
        .bind(a_id)
        .bind(b_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO fabric_pairs
          (pair_name, fabric_kind, computer_a_id, computer_b_id,
           a_iface, a_ip, b_iface, b_ip, endpoints_explicit, verified, status)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,TRUE,FALSE,'pending')
         ON CONFLICT (pair_name) DO UPDATE SET
           fabric_kind=EXCLUDED.fabric_kind, computer_a_id=EXCLUDED.computer_a_id,
           computer_b_id=EXCLUDED.computer_b_id, a_iface=EXCLUDED.a_iface,
           a_ip=EXCLUDED.a_ip, b_iface=EXCLUDED.b_iface, b_ip=EXCLUDED.b_ip,
           endpoints_explicit=TRUE, verified=FALSE, status='pending',
           measured_bandwidth_gbps=NULL, last_probed_at=NULL",
    )
    .bind(&pair_name)
    .bind(&spec.kind)
    .bind(a_id)
    .bind(b_id)
    .bind(&spec.a.iface)
    .bind(spec.a.ip.to_string())
    .bind(&spec.b.iface)
    .bind(spec.b.ip.to_string())
    .execute(&mut *tx)
    .await?;

    if apply {
        tx.commit().await?;
        println!("Applied canonical fabric link {pair_name} ({subnet})");
    } else {
        tx.rollback().await?;
        println!("Dry run: would reconcile canonical fabric link {pair_name} ({subnet})");
    }
    Ok(())
}

async fn computer_id(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, name: &str) -> Result<Uuid> {
    sqlx::query_scalar("SELECT id FROM computers WHERE lower(name) = $1")
        .bind(name)
        .fetch_optional(&mut **tx)
        .await?
        .with_context(|| format!("computer '{name}' not found"))
}

pub async fn handle_fabric_remove(pg: &PgPool, spec: FabricLinkSpec, apply: bool) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ff-fabric-reconcile'))")
        .execute(&mut *tx)
        .await?;
    let a_id = computer_id(&mut tx, &spec.a.node).await?;
    let b_id = computer_id(&mut tx, &spec.b.node).await?;
    // `pair_name` and `endpoints_explicit` cannot identify legacy rows: old
    // materializer hints may have a reversed name and are intentionally marked
    // non-explicit. Removal therefore requires the complete endpoint tuples and
    // kind in either orientation. This remains an operator-requested exact
    // removal; automatic reconcile cleanup stays fenced to legacy rows above.
    let result = sqlx::query(REMOVE_EXACT_LINK_SQL)
        .bind(&spec.kind)
        .bind(a_id)
        .bind(b_id)
        .bind(&spec.a.iface)
        .bind(spec.a.ip.to_string())
        .bind(&spec.b.iface)
        .bind(spec.b.ip.to_string())
        .execute(&mut *tx)
        .await?;
    let count = result.rows_affected();
    if apply {
        tx.commit().await?;
        println!("Removed {count} matching fabric link row(s)");
    } else {
        tx.rollback().await?;
        println!("Dry run: would remove {count} matching fabric link row(s)");
    }
    Ok(())
}

pub async fn handle_fabric_probe(pg: &PgPool, spec: FabricLinkSpec, apply: bool) -> Result<()> {
    spec.validate_link_subnet()?;
    let declared: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM fabric_pairs
         WHERE pair_name=$1 AND endpoints_explicit AND fabric_kind=$2
           AND a_iface=$3 AND a_ip=$4 AND b_iface=$5 AND b_ip=$6)",
    )
    .bind(spec.pair_name())
    .bind(&spec.kind)
    .bind(&spec.a.iface)
    .bind(spec.a.ip.to_string())
    .bind(&spec.b.iface)
    .bind(spec.b.ip.to_string())
    .fetch_one(pg)
    .await?;
    if !declared {
        bail!("no matching explicit canonical link; reconcile it before probing");
    }
    let proof = async {
        probe_endpoint(&spec.a).await?;
        probe_endpoint(&spec.b).await?;
        // The physical path is proven independently of carrier by an IP packet
        // in each direction, sourced from the declared interface.
        probe_ping(&spec.a, &spec.b).await?;
        probe_ping(&spec.b, &spec.a).await
    }
    .await;
    if let Err(error) = proof {
        if apply {
            persist_probe_status(pg, &spec, false, "dead").await?;
        }
        return Err(error);
    }
    println!(
        "Proved both endpoints and bidirectional physical link for {}",
        spec.pair_name()
    );
    if apply {
        persist_probe_status(pg, &spec, true, "verified").await?;
    } else {
        println!("Dry run: verification was not persisted");
    }
    Ok(())
}

async fn persist_probe_status(
    pg: &PgPool,
    spec: &FabricLinkSpec,
    verified: bool,
    status: &str,
) -> Result<()> {
    let result = sqlx::query(
        "UPDATE fabric_pairs
            SET verified=$7, status=$8, last_probed_at=NOW(),
                measured_bandwidth_gbps =
                    CASE WHEN $7 THEN measured_bandwidth_gbps ELSE NULL END
         WHERE pair_name=$1 AND endpoints_explicit AND fabric_kind=$2
           AND a_iface=$3 AND a_ip=$4 AND b_iface=$5 AND b_ip=$6",
    )
    .bind(spec.pair_name())
    .bind(&spec.kind)
    .bind(&spec.a.iface)
    .bind(spec.a.ip.to_string())
    .bind(&spec.b.iface)
    .bind(spec.b.ip.to_string())
    .bind(verified)
    .bind(status)
    .execute(pg)
    .await?;
    if result.rows_affected() != 1 {
        bail!("no matching explicit canonical link; reconcile it before probing");
    }
    Ok(())
}

async fn probe_endpoint(endpoint: &FabricEndpoint) -> Result<()> {
    let node = ff_agent::fleet_info::fetch_node_by_name(&endpoint.node)
        .await
        .map_err(anyhow::Error::msg)?
        .context("computer not found")?;
    let command = format!(
        "ip -o addr show dev '{}' | awk '{{print $4}}' | cut -d/ -f1 | grep -Fx '{}' >/dev/null && test \"$(cat /sys/class/net/'{}'/carrier)\" = 1",
        endpoint.iface, endpoint.ip, endpoint.iface
    );
    let ok = StdCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ServerAliveInterval=3",
            "-o",
            "ServerAliveCountMax=1",
            &format!("{}@{}", node.ssh_user, node.ip),
            &command,
        ])
        .status()?
        .success();
    if !ok {
        bail!(
            "{} endpoint/interface/link-carrier proof failed",
            endpoint.node
        );
    }
    Ok(())
}

async fn probe_ping(from: &FabricEndpoint, to: &FabricEndpoint) -> Result<()> {
    let node = ff_agent::fleet_info::fetch_node_by_name(&from.node)
        .await
        .map_err(anyhow::Error::msg)?
        .context("computer not found")?;
    let ok = StdCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ServerAliveInterval=3",
            "-o",
            "ServerAliveCountMax=1",
            &format!("{}@{}", node.ssh_user, node.ip),
            &format!("ping -I '{}' -c 1 -W 2 '{}'", from.iface, to.ip),
        ])
        .status()?
        .success();
    if !ok {
        bail!("physical link probe {} -> {} failed", from.node, to.node);
    }
    Ok(())
}

pub async fn handle_fabric_pair(pg: &PgPool, a: &str, b: &str, kind: &str) -> Result<()> {
    if a == b {
        bail!("cannot pair a computer with itself");
    }
    let (a_name, b_name) = if a < b { (a, b) } else { (b, a) };
    let pair_name = format!("{a_name}-{b_name}");

    let row_a = sqlx::query("SELECT id FROM computers WHERE name = $1")
        .bind(a_name)
        .fetch_optional(pg)
        .await?
        .with_context(|| format!("computer '{a_name}' not found"))?;
    let row_b = sqlx::query("SELECT id FROM computers WHERE name = $1")
        .bind(b_name)
        .fetch_optional(pg)
        .await?
        .with_context(|| format!("computer '{b_name}' not found"))?;
    let a_id: Uuid = row_a.try_get("id")?;
    let b_id: Uuid = row_b.try_get("id")?;

    sqlx::query(
        "INSERT INTO fabric_pairs \
            (pair_name, fabric_kind, computer_a_id, computer_b_id, \
             a_iface, b_iface, a_ip, b_ip) \
         VALUES ($1, $2, $3, $4, '', '', '', '') \
         ON CONFLICT (pair_name) DO UPDATE SET fabric_kind = EXCLUDED.fabric_kind \
          WHERE NOT fabric_pairs.endpoints_explicit",
    )
    .bind(&pair_name)
    .bind(kind)
    .bind(a_id)
    .bind(b_id)
    .execute(pg)
    .await?;

    println!("Paired: {pair_name} (kind={kind})");
    println!(
        "Next: configure IPs via nmcli on both hosts, then beats will auto-populate iface/ip."
    );
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
    //    vinny↔james TB, etc.).
    let (a_fabric_ip, b_fabric_ip, fabric_kind, iface_a, iface_b, claimed_gbps) =
        resolve_fabric_endpoints(pg, a, b).await?;

    println!(
        "Fabric link: {} ({}) ↔ {} ({}), kind={}{}",
        a,
        a_fabric_ip,
        b,
        b_fabric_ip,
        fabric_kind,
        claimed_gbps
            .map(|g| format!(", claimed={g}Gbps"))
            .unwrap_or_default()
    );

    // 2. Look up SSH user for both nodes.
    let (a_ssh_user, _) = ff_agent::fleet_info::fetch_node_ip_user(a)
        .await
        .with_context(|| format!("could not resolve SSH for {a}"))?;
    let (_b_ssh_user, _) = ff_agent::fleet_info::fetch_node_ip_user(b)
        .await
        .with_context(|| format!("could not resolve SSH for {b}"))?;
    let a_lan_ip = a_ssh_user.clone();
    let _ = a_lan_ip;
    // fetch_node_ip_user returns (ip, ssh_user) tuple — re-fetch for clarity
    let a_meta = ff_agent::fleet_info::fetch_node_by_name(a)
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .with_context(|| format!("computer '{a}' not in fleet"))?;
    let b_meta = ff_agent::fleet_info::fetch_node_by_name(b)
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .with_context(|| format!("computer '{b}' not in fleet"))?;

    let a_target = format!("{}@{}", a_meta.ssh_user, a_meta.ip);
    let b_target = format!("{}@{}", b_meta.ssh_user, b_meta.ip);
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
    let a_is_me = me.eq_ignore_ascii_case(a);
    let b_is_me = me.eq_ignore_ascii_case(b);

    // run_remote_or_local: if `target` matches my node name, run locally
    // (skip SSH); otherwise SSH. Avoids "Connection closed by self" when
    // the operator runs benchmark from one of the endpoints.
    let run_or_local =
        |is_me: bool, target: &str, cmd: &str| -> std::io::Result<std::process::Output> {
            if is_me {
                StdCommand::new("sh").args(["-c", cmd]).output()
            } else {
                StdCommand::new("ssh")
                    .args(["-o", "BatchMode=yes", target, cmd])
                    .output()
            }
        };

    let mut measurements: Vec<(String, f64, Option<i32>)> = Vec::new();

    // 3. Forward direction A → B (unless reverse_only).
    if !reverse_only {
        println!("Starting iperf3 server on {b}...");
        let _ = run_or_local(
            b_is_me,
            &b_target,
            "pkill iperf3 2>/dev/null; iperf3 -s -D --logfile /tmp/iperf3.log",
        );
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        println!("Running iperf3 client on {a} → {b} ({duration}s, {streams} streams)...");
        let cmd = format!("iperf3 -c {b_fabric_ip} -t {duration} -P {streams} -J");
        let out = run_or_local(a_is_me, &a_target, &cmd).context("iperf3 forward failed")?;
        let body = String::from_utf8_lossy(&out.stdout);
        let (gbps, retr) = parse_iperf3_json(&body);
        if gbps > 0.0 {
            println!(
                "  {} → {}: {:.2} Gbps{}",
                a,
                b,
                gbps,
                retr.map(|r| format!(" ({r} retransmits)"))
                    .unwrap_or_default()
            );
            measurements.push(("a_to_b".to_string(), gbps, retr));
        } else {
            println!("  {a} → {b}: failed (no parseable result)");
        }

        let _ = run_or_local(b_is_me, &b_target, "pkill iperf3 2>/dev/null");
    }

    // 4. Reverse direction B → A. Swap the server and client instead of
    //    reusing B's server with `-R`; this gives each direction a fresh server
    //    and works when the reverse data channel is blocked.
    println!("Starting iperf3 server on {a}...");
    let _ = run_or_local(
        a_is_me,
        &a_target,
        "pkill iperf3 2>/dev/null; iperf3 -s -D --logfile /tmp/iperf3.log",
    );
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    println!("Running iperf3 client on {b} → {a} ({duration}s, {streams} streams)...");
    let cmd = format!("iperf3 -c {a_fabric_ip} -t {duration} -P {streams} -J");
    let out = run_or_local(b_is_me, &b_target, &cmd).context("iperf3 reverse failed")?;
    let body = String::from_utf8_lossy(&out.stdout);
    let (gbps, retr) = parse_iperf3_json(&body);
    if gbps > 0.0 {
        println!(
            "  {} → {}: {:.2} Gbps{}",
            b,
            a,
            gbps,
            retr.map(|r| format!(" ({r} retransmits)"))
                .unwrap_or_default()
        );
        measurements.push(("b_to_a".to_string(), gbps, retr));
    } else {
        println!("  {b} → {a}: failed");
    }

    // 5. Stop iperf3 server.
    let _ = run_or_local(a_is_me, &a_target, "pkill iperf3 2>/dev/null");

    // 6. Record measurements.
    let measured_by = ff_agent::fleet_info::resolve_this_worker_name().await;
    let iperf_version = StdCommand::new("iperf3")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(str::to_string));
    for (direction, gbps, retr) in &measurements {
        sqlx::query(
            "INSERT INTO fabric_measurements
                (node_a, node_b, iface_a, iface_b, fabric_kind, direction,
                 streams, duration_secs, measured_gbps, claimed_gbps,
                 retransmits, measured_by, iperf_version)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(a)
        .bind(b)
        .bind(&iface_a)
        .bind(&iface_b)
        .bind(&fabric_kind)
        .bind(direction)
        .bind(streams as i32)
        .bind(duration as i32)
        .bind(*gbps)
        .bind(claimed_gbps)
        .bind(retr.as_ref().copied())
        .bind(&measured_by)
        .bind(iperf_version.as_deref())
        .execute(pg)
        .await?;
    }
    // Roll up the best forward-direction Gbps into fabric_pairs as the
    // canonical "current" measurement so `ff fabric pairs` and the
    // dashboard show fresh data without scanning fabric_measurements.
    let best_gbps = measurements
        .iter()
        .map(|(_, g, _)| *g)
        .fold(0.0_f64, f64::max);
    if best_gbps > 0.0 {
        let pair_name = if a < b {
            format!("{a}-{b}")
        } else {
            format!("{b}-{a}")
        };
        sqlx::query(
            "UPDATE fabric_pairs
                SET measured_bandwidth_gbps = $2,
                    last_probed_at          = NOW()
              WHERE pair_name = $1",
        )
        .bind(&pair_name)
        .bind(best_gbps)
        .execute(pg)
        .await?;
    }

    println!(
        "Recorded {} measurement(s) into fabric_measurements; rolled best={:.2} Gbps into fabric_pairs.",
        measurements.len(),
        best_gbps,
    );
    Ok(())
}

/// `ff fabric benchmark-all` — iterate every row in `fabric_pairs` and
/// run a short benchmark against each. Useful in cron (`@daily ff fabric
/// benchmark-all --duration 5`) to keep `measured_bandwidth_gbps` fresh
/// fleet-wide.
pub async fn handle_fabric_benchmark_all(pg: &PgPool, duration: u32, streams: u32) -> Result<()> {
    let rows = sqlx::query(
        "SELECT pair_name, c_a.name AS a_name, c_b.name AS b_name
           FROM fabric_pairs fp
           JOIN computers c_a ON c_a.id = fp.computer_a_id
           JOIN computers c_b ON c_b.id = fp.computer_b_id
          ORDER BY pair_name",
    )
    .fetch_all(pg)
    .await?;

    if rows.is_empty() {
        println!("(no fabric_pairs rows — nothing to benchmark)");
        return Ok(());
    }

    println!("Benchmarking {} pair(s)...", rows.len());
    let mut ok = 0usize;
    let mut failed = 0usize;
    for r in rows {
        let a: String = r.try_get("a_name")?;
        let b: String = r.try_get("b_name")?;
        println!("\n── {a} ↔ {b} ──");
        match handle_fabric_benchmark(pg, &a, &b, duration, streams, false).await {
            Ok(()) => ok += 1,
            Err(e) => {
                eprintln!("  ! {a}↔{b} failed: {e}");
                failed += 1;
            }
        }
    }
    println!("\nbenchmark-all summary: ok={ok} failed={failed}");
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
              LIMIT $3",
        )
        .bind(an)
        .bind(bn)
        .bind(limit)
        .fetch_all(pg)
        .await?
    } else {
        sqlx::query(
            "SELECT measured_at, node_a, node_b, fabric_kind, direction,
                    measured_gbps, claimed_gbps, retransmits
               FROM fabric_measurements
              ORDER BY measured_at DESC
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pg)
        .await?
    };

    if rows.is_empty() {
        println!("(no measurements yet — run `ff fabric benchmark <a> <b>` to populate)");
        return Ok(());
    }

    println!(
        "{:<22} {:<24} {:<14} {:<10} {:>8} {:>9} {:>7}",
        "MEASURED_AT", "PAIR", "FABRIC", "DIRECTION", "Gbps", "CLAIMED", "RETR"
    );
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
        println!(
            "{:<22} {:<24} {:<14} {:<10} {:>8.2} {:>9} {:>7}",
            measured_at.format("%Y-%m-%d %H:%M:%S"),
            format!("{}↔{}", na, nb),
            kind,
            dir,
            gbps,
            claimed
                .map(|c| format!("{c}Gbps"))
                .unwrap_or_else(|| "-".into()),
            retr.map(|r| r.to_string()).unwrap_or_else(|| "-".into())
        );
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
    let a_ips_raw: Option<(Value,)> =
        sqlx::query_as("SELECT all_ips FROM computers WHERE name = $1")
            .bind(a)
            .fetch_optional(pg)
            .await?;
    let b_ips_raw: Option<(Value,)> =
        sqlx::query_as("SELECT all_ips FROM computers WHERE name = $1")
            .bind(b)
            .fetch_optional(pg)
            .await?;

    let a_ips = a_ips_raw
        .with_context(|| format!("computer '{a}' not in DB"))?
        .0;
    let b_ips = b_ips_raw
        .with_context(|| format!("computer '{b}' not in DB"))?
        .0;

    let parse = |v: &Value| -> Vec<(String, String, String, Option<u32>)> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|item| {
                        let ip = item.get("ip")?.as_str()?.to_string();
                        let iface = item.get("iface")?.as_str()?.to_string();
                        let kind = item.get("kind")?.as_str()?.to_string();
                        let speed = item
                            .get("link_speed_gbps")
                            .and_then(|x| x.as_u64().map(|n| n as u32));
                        Some((ip, iface, kind, speed))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let a_list = parse(&a_ips);
    let b_list = parse(&b_ips);

    // Match by shared /24 subnet on a fabric kind.
    for (aip, aif, akind, aspeed) in &a_list {
        if !akind.ends_with("-fabric") {
            continue;
        }
        let prefix: String = aip.rsplit_once('.').map(|x| x.0).unwrap_or("").to_string();
        if prefix.is_empty() {
            continue;
        }
        for (bip, bif, bkind, _) in &b_list {
            if bkind != akind {
                continue;
            }
            let bprefix: String = bip.rsplit_once('.').map(|x| x.0).unwrap_or("").to_string();
            if prefix == bprefix && aip != bip {
                return Ok((
                    aip.clone(),
                    bip.clone(),
                    akind.clone(),
                    aif.clone(),
                    bif.clone(),
                    aspeed.map(|s| s as i32),
                ));
            }
        }
    }
    bail!(
        "no shared fabric subnet found between {a} and {b}; are both ends configured? \
           (run `ff fleet status` and verify all_ips on each computer)"
    )
}

/// Parse iperf3 -J JSON output. Returns (Gbps, retransmits).
fn parse_iperf3_json(body: &str) -> (f64, Option<i32>) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (0.0, None),
    };
    // sum_sent.bits_per_second is the canonical end-of-test result.
    let bps = v
        .get("end")
        .and_then(|e| e.get("sum_sent"))
        .and_then(|s| s.get("bits_per_second"))
        .and_then(|n| n.as_f64())
        .unwrap_or(0.0);
    let retr = v
        .get("end")
        .and_then(|e| e.get("sum_sent"))
        .and_then(|s| s.get("retransmits"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);
    (bps / 1e9, retr)
}

#[cfg(test)]
mod ring_tests {
    use std::collections::HashSet;

    use super::*;

    fn compact_sql(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn link(a: &str, ai: &str, aip: &str, b: &str, bi: &str, bip: &str) -> FabricLinkSpec {
        FabricLinkSpec::new(a, ai, aip, b, bi, bip, "cx7-200g").unwrap()
    }

    fn assert_topology_is_unambiguous(links: &[FabricLinkSpec]) -> Result<()> {
        let mut endpoints = HashSet::new();
        let mut subnets = HashSet::new();
        for link in links {
            for endpoint in [&link.a, &link.b] {
                if !endpoints.insert((endpoint.node.clone(), endpoint.iface.clone())) {
                    bail!("duplicate endpoint");
                }
            }
            if !subnets.insert(link.validate_link_subnet()?) {
                bail!("duplicate subnet");
            }
        }
        Ok(())
    }

    #[test]
    fn supports_two_independent_three_node_rings() {
        let rings = [
            link("a", "cx0", "10.80.0.1", "b", "cx0", "10.80.0.2"),
            link("b", "cx1", "10.80.0.5", "c", "cx0", "10.80.0.6"),
            link("c", "cx1", "10.80.0.9", "a", "cx1", "10.80.0.10"),
            link("d", "cx0", "10.81.0.1", "e", "cx0", "10.81.0.2"),
            link("e", "cx1", "10.81.0.5", "f", "cx0", "10.81.0.6"),
            link("f", "cx1", "10.81.0.9", "d", "cx1", "10.81.0.10"),
        ];
        assert_topology_is_unambiguous(&rings).unwrap();
    }

    #[test]
    fn reversed_orientation_has_one_canonical_identity() {
        let forward = link("sia", "cx0", "10.90.0.1", "adele", "cx1", "10.90.0.2");
        let reversed = link("ADELE", "cx1", "10.90.0.2", "SIA", "cx0", "10.90.0.1");
        assert_eq!(forward, reversed);
        assert_eq!(forward.pair_name(), "adele-sia");
    }

    #[test]
    fn rejects_crossed_endpoint_and_duplicate_subnet_rows() {
        let base = link("a", "cx0", "10.91.0.1", "b", "cx0", "10.91.0.2");
        let crossed = link("a", "cx0", "10.92.0.1", "c", "cx0", "10.92.0.2");
        assert!(assert_topology_is_unambiguous(&[base.clone(), crossed]).is_err());
        let duplicate_subnet = link("c", "cx0", "10.91.0.1", "d", "cx0", "10.91.0.2");
        assert!(assert_topology_is_unambiguous(&[base, duplicate_subnet]).is_err());
    }

    #[test]
    fn rejects_network_and_broadcast_addresses_for_slash_30_links() {
        let network = link("a", "cx0", "10.91.0.0", "b", "cx0", "10.91.0.1");
        assert!(network.validate_link_subnet().is_err());
        let broadcast = link("a", "cx0", "10.91.0.2", "b", "cx0", "10.91.0.3");
        assert!(broadcast.validate_link_subnet().is_err());
    }

    #[test]
    fn reconcile_ignores_legacy_hints_but_fails_on_identity_collisions() {
        let sql = compact_sql(RECONCILE_CONFLICT_SQL);
        assert!(sql.contains("WHERE (pair_name = $1 AND NOT"));
        assert!(sql.contains("OR (endpoints_explicit AND pair_name <> $1 AND"));
        assert!(sql.contains(
            "(computer_a_id = $2 AND computer_b_id = $4) OR (computer_a_id = $4 AND computer_b_id = $2)"
        ));
        assert!(sql.contains("OR cidr = $8"));
    }

    #[test]
    fn reconcile_legacy_cleanup_is_same_pair_and_non_explicit_only() {
        let sql = compact_sql(RECONCILE_LEGACY_CLEANUP_SQL);
        assert!(sql.contains("WHERE NOT endpoints_explicit AND pair_name <> $1"));
        assert!(sql.contains(
            "(computer_a_id = $2 AND computer_b_id = $3) OR (computer_a_id = $3 AND computer_b_id = $2)"
        ));
        assert!(!sql.contains("a_iface"));
        assert!(!sql.contains("a_ip"));
        assert!(!sql.contains("cidr"));
        assert!(!sql.contains("$4"));
    }

    #[test]
    fn remove_requires_exact_endpoint_tuples_in_either_orientation() {
        let sql = compact_sql(REMOVE_EXACT_LINK_SQL);
        assert!(!sql.contains("pair_name"));
        assert!(!sql.contains("endpoints_explicit"));
        assert!(sql.contains(
            "computer_a_id=$2 AND computer_b_id=$3 AND a_iface=$4 AND a_ip=$5 AND b_iface=$6 AND b_ip=$7"
        ));
        assert!(sql.contains(
            "computer_a_id=$3 AND computer_b_id=$2 AND a_iface=$6 AND a_ip=$7 AND b_iface=$4 AND b_ip=$5"
        ));
    }
}
