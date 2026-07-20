//! Fleet-wide SSH mesh verification + propagation.
//! See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md §3h.

use std::collections::HashSet;
use std::time::Duration;

use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::timeout;

const SKIPPED_COMPUTER_STATUSES: [&str; 3] = ["offline", "reserved", "decommissioned"];

fn mesh_eligible(node: &ff_db::FleetNodeRow) -> bool {
    computer_status_eligible(node.computer_status.as_deref())
}

fn computer_status_eligible(status: Option<&str>) -> bool {
    !status.is_some_and(|status| SKIPPED_COMPUTER_STATUSES.contains(&status))
}

fn retry_cap_reached(
    attempts: impl Iterator<Item = (chrono::DateTime<chrono::Utc>, i32)>,
    window_start: chrono::DateTime<chrono::Utc>,
) -> bool {
    attempts
        .filter(|(created_at, _)| *created_at >= window_start)
        .map(|(_, attempts)| attempts.max(1))
        .sum::<i32>()
        >= 5
}

async fn mark_ineligible_pairs_skipped(
    pool: &PgPool,
    nodes: &[ff_db::FleetNodeRow],
) -> Result<(), String> {
    let names: Vec<&str> = nodes
        .iter()
        .filter(|node| !mesh_eligible(node))
        .map(|node| node.name.as_str())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE fleet_mesh_status
            SET status = 'skipped', last_checked = NOW(),
                last_error = 'endpoint computer is offline, reserved, or decommissioned'
          WHERE src_node = ANY($1) OR dst_node = ANY($1)",
    )
    .bind(&names)
    .execute(pool)
    .await
    .map_err(|e| format!("mark skipped mesh rows: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct MeshCell {
    pub src: String,
    pub dst: String,
    pub status: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MeshMatrix {
    pub cells: Vec<MeshCell>,
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

pub async fn pairwise_ssh_check(pool: &PgPool) -> Result<MeshMatrix, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes).await?;
    pairwise_ssh_check_inner(pool, &nodes, None).await
}

pub async fn pairwise_ssh_check_node(pool: &PgPool, node: &str) -> Result<MeshMatrix, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes).await?;
    pairwise_ssh_check_inner(pool, &nodes, Some(node)).await
}

async fn pairwise_ssh_check_inner(
    pool: &PgPool,
    nodes: &[ff_db::FleetNodeRow],
    only_node: Option<&str>,
) -> Result<MeshMatrix, String> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let by_name: std::collections::HashMap<String, (String, String)> = nodes
        .iter()
        .map(|n| (n.name.clone(), (n.ssh_user.clone(), n.ip.clone())))
        .collect();

    let mut pairs: Vec<(String, String, String, String, String)> = Vec::new();
    for src in nodes {
        for dst in nodes {
            if src.name == dst.name || !mesh_eligible(src) || !mesh_eligible(dst) {
                continue;
            }
            if let Some(n) = only_node
                && src.name != n
                && dst.name != n
            {
                continue;
            }
            pairs.push((
                src.name.clone(),
                src.ssh_user.clone(),
                src.ip.clone(),
                dst.name.clone(),
                by_name
                    .get(&dst.name)
                    .map(|(u, _)| u.clone())
                    .unwrap_or_default(),
            ));
            let _ = dst;
        }
    }
    let _ = by_name;

    let mut futs = FuturesUnordered::new();
    let mut cells = Vec::with_capacity(pairs.len());
    for (src, src_user, src_ip, dst, dst_user) in pairs {
        let dst_ip = nodes
            .iter()
            .find(|n| n.name == dst)
            .map(|n| n.ip.clone())
            .unwrap_or_default();
        futs.push(probe_pair(src, src_user, src_ip, dst, dst_user, dst_ip));
        if futs.len() >= 8
            && let Some(cell) = futs.next().await
        {
            let _ = ff_db::pg_upsert_mesh_status(
                pool,
                &cell.src,
                &cell.dst,
                &cell.status,
                cell.last_error.as_deref(),
            )
            .await;
            cells.push(cell);
        }
    }
    while let Some(cell) = futs.next().await {
        let _ = ff_db::pg_upsert_mesh_status(
            pool,
            &cell.src,
            &cell.dst,
            &cell.status,
            cell.last_error.as_deref(),
        )
        .await;
        cells.push(cell);
    }

    Ok(MeshMatrix {
        cells,
        checked_at: chrono::Utc::now(),
    })
}

async fn probe_pair(
    src: String,
    src_user: String,
    src_ip: String,
    dst: String,
    dst_user: String,
    dst_ip: String,
) -> MeshCell {
    // Bypass the (possibly wedged) inherited ssh-agent on both hops — the outer
    // hop is daemon-spawned, the inner runs in the src host's shell. See
    // `crate::ssh_opts`.
    let ssh_bypass = crate::ssh_opts::SSH_AGENT_BYPASS;
    let inner = format!(
        "ssh {ssh_bypass} -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {dst_user}@{dst_ip} true"
    );
    let result = timeout(
        Duration::from_secs(12),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &format!("{src_user}@{src_ip}"),
                &inner,
            ])
            .output(),
    )
    .await;

    match result {
        Ok(Ok(out)) if out.status.success() => MeshCell {
            src,
            dst,
            status: "ok".into(),
            last_error: None,
        },
        Ok(Ok(out)) => MeshCell {
            src,
            dst,
            status: "failed".into(),
            last_error: Some(format!(
                "exit {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .take(120)
                    .collect::<String>()
            )),
        },
        Ok(Err(e)) => MeshCell {
            src,
            dst,
            status: "failed".into(),
            last_error: Some(format!("spawn: {e}")),
        },
        Err(_) => MeshCell {
            src,
            dst,
            status: "failed".into(),
            last_error: Some("timeout".into()),
        },
    }
}

/// One direct (this node → dst) reachability probe: ICMP ping + single-hop SSH.
#[derive(Debug, Clone)]
pub struct LocalProbe {
    pub src: String,
    pub dst: String,
    pub ip: String,
    pub ping_ok: bool,
    pub ssh_ok: bool,
    /// "ok" | "failed" — what gets stored in fleet_mesh_status.
    pub status: String,
    pub detail: Option<String>,
}

/// Direct reachability fan-out FROM this node: ping + single-hop SSH
/// (BatchMode, ConnectTimeout=5) to every other `fleet_workers` row. Unlike
/// the pairwise N×N check this needs no intermediate hop, so it still answers
/// "who went dark?" when this node is the only reachable one, and the ping
/// column separates host-down / stale-IP from host-up-but-SSH-broken.
/// Results are upserted into fleet_mesh_status as (this node → dst) rows so
/// failures land on the same alert path the integrity sweep reads.
pub async fn local_reach_check(
    pool: &PgPool,
    only_node: Option<&str>,
) -> Result<Vec<LocalProbe>, String> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let me = crate::fleet_info::resolve_this_worker_name().await;
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes).await?;

    let mut futs = FuturesUnordered::new();
    let mut probes = Vec::new();
    for n in nodes.iter().filter(|n| n.name != me && mesh_eligible(n)) {
        if let Some(o) = only_node
            && n.name != o
        {
            continue;
        }
        futs.push(probe_direct(
            me.clone(),
            n.name.clone(),
            n.ssh_user.clone(),
            n.ip.clone(),
        ));
        if futs.len() >= 8
            && let Some(p) = futs.next().await
        {
            let _ =
                ff_db::pg_upsert_mesh_status(pool, &p.src, &p.dst, &p.status, p.detail.as_deref())
                    .await;
            probes.push(p);
        }
    }
    while let Some(p) = futs.next().await {
        let _ = ff_db::pg_upsert_mesh_status(pool, &p.src, &p.dst, &p.status, p.detail.as_deref())
            .await;
        probes.push(p);
    }
    probes.sort_by(|a, b| a.dst.cmp(&b.dst));
    Ok(probes)
}

async fn probe_direct(src: String, dst: String, dst_user: String, dst_ip: String) -> LocalProbe {
    // macOS ping -W is milliseconds; Linux is seconds.
    let ping_wait: &str = if cfg!(target_os = "macos") {
        "2000"
    } else {
        "2"
    };
    let ping_ok = matches!(
        timeout(
            Duration::from_secs(4),
            Command::new("ping")
                .args(["-c", "1", "-W", ping_wait, &dst_ip])
                .output(),
        )
        .await,
        Ok(Ok(o)) if o.status.success()
    );

    let ssh_res = timeout(
        Duration::from_secs(8),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &format!("{dst_user}@{dst_ip}"),
                "true",
            ])
            .output(),
    )
    .await;
    let ssh_err = match ssh_res {
        Ok(Ok(out)) if out.status.success() => None,
        Ok(Ok(out)) => Some(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(120)
                .collect::<String>()
        )),
        Ok(Err(e)) => Some(format!("spawn: {e}")),
        Err(_) => Some("timeout".into()),
    };
    let ssh_ok = ssh_err.is_none();
    let (status, detail) = classify_direct_probe(ping_ok, ssh_err);
    LocalProbe {
        src,
        dst,
        ip: dst_ip,
        ping_ok,
        ssh_ok,
        status,
        detail,
    }
}

/// Fold a ping result + optional SSH failure into the (status, last_error)
/// pair stored in fleet_mesh_status. SSH decides ok/failed — ping is
/// diagnostic (ICMP can be blocked while SSH works, and vice versa).
fn classify_direct_probe(ping_ok: bool, ssh_err: Option<String>) -> (String, Option<String>) {
    match (ping_ok, ssh_err) {
        (true, None) => ("ok".into(), None),
        (false, None) => (
            "ok".into(),
            Some("ssh ok; ping failed (icmp blocked or lossy path)".into()),
        ),
        (ping_ok, Some(e)) => (
            "failed".into(),
            Some(format!(
                "ping {}; ssh {e}",
                if ping_ok { "ok" } else { "failed" }
            )),
        ),
    }
}

pub async fn mesh_propagate(
    pool: &PgPool,
    params: &serde_json::Value,
) -> Result<(usize, usize), String> {
    let new_node = params
        .get("new_node")
        .and_then(|v| v.as_str())
        .ok_or("missing new_node")?;
    let new_ip = params
        .get("new_node_ip")
        .and_then(|v| v.as_str())
        .ok_or("missing new_node_ip")?;
    let new_user = params
        .get("new_node_ssh_user")
        .and_then(|v| v.as_str())
        .ok_or("missing new_node_ssh_user")?;
    let user_key = params
        .get("user_public_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let host_keys: Vec<String> = params
        .get("host_public_keys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let known_lines: Vec<String> = host_keys
        .iter()
        .filter(|k| !k.trim().is_empty())
        .map(|k| format!("{new_ip},{new_node} {k}"))
        .collect();

    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes).await?;
    if nodes
        .iter()
        .find(|node| node.name == new_node)
        .is_some_and(|node| !mesh_eligible(node))
    {
        return Ok((0, 0));
    }
    let mut ok = 0usize;
    let mut fail = 0usize;
    for peer in &nodes {
        if peer.name == new_node || !mesh_eligible(peer) {
            continue;
        }
        match propagate_to_peer(peer, user_key, &known_lines, new_user, new_ip).await {
            Ok(()) => {
                ok += 1;
                let _ = ff_db::pg_upsert_mesh_status(pool, &peer.name, new_node, "ok", None).await;
                let _ = ff_db::pg_upsert_mesh_status(pool, new_node, &peer.name, "ok", None).await;
            }
            Err(e) => {
                fail += 1;
                let _ =
                    ff_db::pg_upsert_mesh_status(pool, &peer.name, new_node, "failed", Some(&e))
                        .await;
            }
        }
    }
    Ok((ok, fail))
}

async fn propagate_to_peer(
    peer: &ff_db::FleetNodeRow,
    user_key: &str,
    known_lines: &[String],
    new_user: &str,
    new_ip: &str,
) -> Result<(), String> {
    let peer_dest = format!("{}@{}", peer.ssh_user, peer.ip);
    if !user_key.trim().is_empty() {
        let cmd = format!(
            "mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
             grep -Fq {quoted} ~/.ssh/authorized_keys || \
             echo {quoted} >> ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys",
            quoted = shell_escape_single(user_key),
        );
        ssh_exec(&peer_dest, &cmd).await?;
    }
    for line in known_lines {
        let cmd = format!(
            "touch ~/.ssh/known_hosts && \
             grep -Fq {quoted} ~/.ssh/known_hosts || \
             echo {quoted} >> ~/.ssh/known_hosts && \
             chmod 644 ~/.ssh/known_hosts",
            quoted = shell_escape_single(line),
        );
        ssh_exec(&peer_dest, &cmd).await?;
    }
    let probe = format!(
        "ssh {} -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {new_user}@{new_ip} true",
        crate::ssh_opts::SSH_AGENT_BYPASS,
    );
    ssh_exec(&peer_dest, &probe).await
}

async fn ssh_exec(dest: &str, cmd: &str) -> Result<(), String> {
    let out = timeout(
        Duration::from_secs(15),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=accept-new",
                dest,
                cmd,
            ])
            .output(),
    )
    .await
    .map_err(|_| format!("ssh to {dest} timed out"))?
    .map_err(|e| format!("ssh spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    Ok(())
}

fn shell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Re-probe a single (src, dst) pair and upsert the result. Used by the
/// `mesh_retry` deferred task when an auto-retry fires.
pub async fn probe_single_pair(pool: &PgPool, src: &str, dst: &str) -> Result<MeshCell, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes).await?;
    let s = nodes
        .iter()
        .find(|n| n.name == src)
        .ok_or_else(|| format!("src node '{src}' not in fleet_workers"))?;
    let d = nodes
        .iter()
        .find(|n| n.name == dst)
        .ok_or_else(|| format!("dst node '{dst}' not in fleet_workers"))?;
    if !mesh_eligible(s) || !mesh_eligible(d) {
        return Ok(MeshCell {
            src: src.to_string(),
            dst: dst.to_string(),
            status: "skipped".into(),
            last_error: Some("endpoint computer is offline, reserved, or decommissioned".into()),
        });
    }
    let cell = probe_pair(
        s.name.clone(),
        s.ssh_user.clone(),
        s.ip.clone(),
        d.name.clone(),
        d.ssh_user.clone(),
        d.ip.clone(),
    )
    .await;
    let _ = ff_db::pg_upsert_mesh_status(
        pool,
        &cell.src,
        &cell.dst,
        &cell.status,
        cell.last_error.as_deref(),
    )
    .await;
    Ok(cell)
}

/// For every `fleet_mesh_status` row in status='failed' whose last_checked is
/// older than 10 minutes, enqueue a `mesh_retry` deferred task — de-duplicated
/// against any active retry for the same (src,dst) pair. Capped at 5 attempts
/// per 24h across task IDs so a completed task cannot reset the retry budget.
pub async fn enqueue_retries(pool: &PgPool) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
    let retry_window = chrono::Utc::now() - chrono::Duration::hours(24);
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes).await?;
    let eligible: HashSet<&str> = nodes
        .iter()
        .filter(|node| mesh_eligible(node))
        .map(|node| node.name.as_str())
        .collect();
    let rows = ff_db::pg_list_mesh_status(pool, None)
        .await
        .map_err(|e| format!("pg_list_mesh_status: {e}"))?;
    let stale: Vec<(String, String)> = rows
        .iter()
        .filter(|r| {
            r.status == "failed"
                && eligible.contains(r.src_node.as_str())
                && eligible.contains(r.dst_node.as_str())
                && r.last_checked.map(|t| t < cutoff).unwrap_or(true)
        })
        .map(|r| (r.src_node.clone(), r.dst_node.clone()))
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let existing = ff_db::pg_list_deferred(pool, None, 500)
        .await
        .map_err(|e| format!("pg_list_deferred: {e}"))?;
    let mut created = 0;
    for (src, dst) in stale {
        let matching: Vec<_> = existing
            .iter()
            .filter(|t| {
                t.kind == "mesh_retry"
                    && t.payload.get("src").and_then(|v| v.as_str()) == Some(&src)
                    && t.payload.get("dst").and_then(|v| v.as_str()) == Some(&dst)
            })
            .collect();
        let active = matching.iter().any(|t| {
            matches!(
                t.status.as_str(),
                "pending" | "dispatchable" | "claimed" | "running"
            )
        });
        let capped = retry_cap_reached(
            matching.iter().map(|t| (t.created_at, t.attempts)),
            retry_window,
        );
        if active || capped {
            continue;
        }
        let title = format!("Mesh retry {src} → {dst}");
        let payload = serde_json::json!({ "src": src, "dst": dst });
        let trig = serde_json::json!({});
        let caps = serde_json::json!([]);
        if ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "mesh_retry",
            &payload,
            "operator",
            &trig,
            Some("taylor"),
            &caps,
            Some("mesh_auto_retry"),
            Some(5),
        )
        .await
        .is_ok()
        {
            created += 1;
        }
    }
    Ok(created)
}

pub async fn refresh_stale(pool: &PgPool, max_age: chrono::Duration) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - max_age;
    let all = ff_db::pg_list_mesh_status(pool, None)
        .await
        .map_err(|e| format!("pg_list_mesh_status: {e}"))?;
    let stale: HashSet<(String, String)> = all
        .iter()
        .filter(|r| r.last_checked.map(|t| t < cutoff).unwrap_or(true))
        .map(|r| (r.src_node.clone(), r.dst_node.clone()))
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let _ = pairwise_ssh_check(pool).await?;
    Ok(stale.len())
}

/// Spawn the leader-gated mesh-refresh loop: every `interval_secs`, re-probe SSH
/// mesh pairs whose stored status is older than `max_age_hours` so
/// `fleet_ssh_mesh` reflects reality. Without this, a pair recorded as `failed`
/// while a node was briefly unreachable (e.g. mid-deploy) stays `failed`
/// FOREVER — the integrity `mesh_ssh_complete` check then reports a node
/// degraded long after SSH recovered (observed: sia↔adele stale-failed though
/// both directions worked by IP). Same legacy-only gap as the version-check tick
/// (#396): mesh probing ran only on-demand / in the legacy `ff daemon`, never in
/// forgefleetd. Leader-gated — it's a fleet-wide probe orchestrated from one
/// node, not per-node.
pub fn spawn_mesh_refresh_tick(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    max_age_hours: i64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    match refresh_stale(&pg, chrono::Duration::hours(max_age_hours)).await {
                        Ok(n) if n > 0 => {
                            tracing::info!(stale = n, "mesh-refresh: re-probed stale mesh pairs")
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "mesh-refresh tick failed"),
                    }
                    // After refreshing, classify the matrix and fire the
                    // ssh_mesh_asymmetric alert on asymmetric / symmetric-failed
                    // pairs so drift surfaces instead of sitting silently in
                    // fleet_mesh_status.
                    match evaluate_and_alert(&pg).await {
                        Ok(r) if !r.is_empty() => tracing::info!(
                            asymmetric = r.asymmetric.len(),
                            symmetric_failed = r.symmetric_failed.len(),
                            "mesh-refresh: fired ssh_mesh_asymmetric alert"
                        ),
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "mesh-refresh: alert evaluation failed")
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::info!("mesh-refresh tick loop stopped");
    })
}

/// The alert policy fired on asymmetric / symmetric-failed mesh pairs.
const MESH_ALERT_POLICY: &str = "ssh_mesh_asymmetric";

/// One problem pair, canonicalized so `a <= b`. `ab` is the recorded status of
/// the a→b probe, `ba` of b→a; `detail` folds both directions' last_error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshPair {
    pub a: String,
    pub b: String,
    pub ab: String,
    pub ba: String,
    pub detail: Option<String>,
}

/// The two failure classes the alert distinguishes. Asymmetric (one direction
/// ok, the other failed) is the stale-IP / stale-key signature — NOT a downed
/// host — and is exactly what made the "marcus unreachable" alarm a false
/// positive. Symmetric-failed (both directions dead) is a genuinely
/// unreachable host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeshAlertReport {
    pub asymmetric: Vec<MeshPair>,
    pub symmetric_failed: Vec<MeshPair>,
}

impl MeshAlertReport {
    pub fn is_empty(&self) -> bool {
        self.asymmetric.is_empty() && self.symmetric_failed.is_empty()
    }
    pub fn total(&self) -> usize {
        self.asymmetric.len() + self.symmetric_failed.len()
    }
}

fn combine_detail(ab_err: Option<&str>, ba_err: Option<&str>) -> Option<String> {
    match (ab_err, ba_err) {
        (Some(x), Some(y)) => Some(format!("→ {x} | ← {y}")),
        (Some(x), None) => Some(format!("→ {x}")),
        (None, Some(y)) => Some(format!("← {y}")),
        (None, None) => None,
    }
}

/// Pure classifier over the current `fleet_mesh_status` rows: for every node
/// pair whose BOTH directions were probed to a terminal status (`ok`/`failed`),
/// bucket it as asymmetric (exactly one direction failed) or symmetric-failed
/// (both failed). Pairs with a missing/`pending`/`skipped` direction are left
/// out — asymmetry is only meaningful when both directions are known.
pub fn classify_mesh_pairs(rows: &[ff_db::MeshStatusRow]) -> MeshAlertReport {
    use std::collections::HashMap;
    let by_pair: HashMap<(&str, &str), (&str, Option<&str>)> = rows
        .iter()
        .map(|r| {
            (
                (r.src_node.as_str(), r.dst_node.as_str()),
                (r.status.as_str(), r.last_error.as_deref()),
            )
        })
        .collect();

    let terminal = |s: &str| s == "ok" || s == "failed";
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut report = MeshAlertReport::default();
    for r in rows {
        let (a, b) = (r.src_node.as_str(), r.dst_node.as_str());
        let (x, y) = if a <= b { (a, b) } else { (b, a) };
        if !seen.insert((x, y)) {
            continue;
        }
        let (Some(&(xy_status, xy_err)), Some(&(yx_status, yx_err))) =
            (by_pair.get(&(x, y)), by_pair.get(&(y, x)))
        else {
            continue;
        };
        if !terminal(xy_status) || !terminal(yx_status) {
            continue;
        }
        let pair = MeshPair {
            a: x.to_string(),
            b: y.to_string(),
            ab: xy_status.to_string(),
            ba: yx_status.to_string(),
            detail: combine_detail(xy_err, yx_err),
        };
        match (xy_status == "failed", yx_status == "failed") {
            (true, true) => report.symmetric_failed.push(pair),
            (true, false) | (false, true) => report.asymmetric.push(pair),
            (false, false) => {}
        }
    }
    report
        .asymmetric
        .sort_by(|p, q| (&p.a, &p.b).cmp(&(&q.a, &q.b)));
    report
        .symmetric_failed
        .sort_by(|p, q| (&p.a, &p.b).cmp(&(&q.a, &q.b)));
    report
}

/// Human-readable alert body (pure, so it's unit-testable). Leads with the
/// asymmetric interpretation because that's the diagnostic that would have
/// short-circuited the stale-IP false alarm.
pub fn mesh_alert_message(report: &MeshAlertReport) -> String {
    let mut parts = Vec::new();
    if !report.asymmetric.is_empty() {
        let list: Vec<String> = report
            .asymmetric
            .iter()
            .map(|p| format!("{}↔{}", p.a, p.b))
            .collect();
        parts.push(format!(
            "{} asymmetric pair(s) [one direction failed → stale IP/key, NOT a down host]: {}",
            report.asymmetric.len(),
            list.join(", ")
        ));
    }
    if !report.symmetric_failed.is_empty() {
        let list: Vec<String> = report
            .symmetric_failed
            .iter()
            .map(|p| format!("{}↔{}", p.a, p.b))
            .collect();
        parts.push(format!(
            "{} unreachable pair(s) [both directions failed]: {}",
            report.symmetric_failed.len(),
            list.join(", ")
        ));
    }
    format!(
        "SSH mesh check: {}. Inspect a pair with `ff fleet ssh-mesh-check --node <name>`; \
         asymmetric pairs usually mean a stale IP in fleet_workers or a missing key, not a downed host.",
        parts.join("; ")
    )
}

/// Read the current mesh matrix, classify it, and — if any asymmetric or
/// symmetric-failed pairs exist — fire the `ssh_mesh_asymmetric` alert. Both
/// the leader-gated refresh tick and the on-demand `ff fleet ssh-mesh-check`
/// verb call this; it returns the report regardless so callers can render it.
/// Alerting is best-effort and never fails the probe.
pub async fn evaluate_and_alert(pool: &PgPool) -> Result<MeshAlertReport, String> {
    let rows = ff_db::pg_list_mesh_status(pool, None)
        .await
        .map_err(|e| format!("pg_list_mesh_status: {e}"))?;
    let report = classify_mesh_pairs(&rows);
    if !report.is_empty() {
        fire_mesh_alert(pool, &report).await;
    }
    Ok(report)
}

/// Load the seeded `ssh_mesh_asymmetric` policy, dispatch through its channel,
/// then record the `alert_events` row — same shape as
/// [`crate::fleet_integrity`]'s degraded-member alert. No-op if the policy is
/// missing/disabled.
async fn fire_mesh_alert(pool: &PgPool, report: &MeshAlertReport) {
    let policy: Option<(uuid::Uuid, String, String)> = match sqlx::query_as(
        "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
    )
    .bind(MESH_ALERT_POLICY)
    .fetch_optional(pool)
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "ssh-mesh: failed to load {MESH_ALERT_POLICY} policy");
            None
        }
    };
    let Some((policy_id, severity, channel)) = policy else {
        tracing::error!(
            "ssh-mesh: {} problem pair(s) but alert policy '{}' missing/disabled — NOT alerting",
            report.total(),
            MESH_ALERT_POLICY
        );
        return;
    };

    let message = mesh_alert_message(report);
    // Dispatch FIRST so the recorded channel_result reflects reality.
    let channel_result =
        crate::alert_evaluator::dispatch_alert(pool, &channel, &severity, &message).await;

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO alert_events
            (policy_id, computer_id, value, value_text, message, channel_result)
        VALUES ($1, NULL, $2, NULL, $3, $4)
        "#,
    )
    .bind(policy_id)
    .bind(report.total() as f64)
    .bind(&message)
    .bind(&channel_result)
    .execute(pool)
    .await
    {
        tracing::error!(error = %e, "ssh-mesh: failed to record alert_event");
    }

    tracing::warn!(
        asymmetric = report.asymmetric.len(),
        symmetric_failed = report.symmetric_failed.len(),
        channel = %channel,
        channel_result = %channel_result,
        "ssh-mesh: asymmetric/failed-pair alert fired"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(src: &str, dst: &str, status: &str, err: Option<&str>) -> ff_db::MeshStatusRow {
        ff_db::MeshStatusRow {
            src_node: src.to_string(),
            dst_node: dst.to_string(),
            status: status.to_string(),
            last_checked: None,
            last_error: err.map(str::to_string),
            attempts: 1,
        }
    }

    #[test]
    fn asymmetric_pair_is_flagged_not_symmetric() {
        let rows = vec![
            row("marcus", "sia", "failed", Some("timeout")),
            row("sia", "marcus", "ok", None),
        ];
        let r = classify_mesh_pairs(&rows);
        assert_eq!(r.asymmetric.len(), 1);
        assert!(r.symmetric_failed.is_empty());
        let p = &r.asymmetric[0];
        assert_eq!((p.a.as_str(), p.b.as_str()), ("marcus", "sia"));
    }

    #[test]
    fn both_directions_failed_is_symmetric_unreachable() {
        let rows = vec![
            row("a", "b", "failed", Some("timeout")),
            row("b", "a", "failed", Some("refused")),
        ];
        let r = classify_mesh_pairs(&rows);
        assert!(r.asymmetric.is_empty());
        assert_eq!(r.symmetric_failed.len(), 1);
        assert_eq!(
            r.symmetric_failed[0].detail.as_deref(),
            Some("→ timeout | ← refused")
        );
    }

    #[test]
    fn both_ok_and_partial_pairs_are_ignored() {
        let rows = vec![
            row("a", "b", "ok", None),
            row("b", "a", "ok", None),
            // only one direction known → cannot judge asymmetry
            row("c", "d", "failed", Some("x")),
            // reverse direction still pending → not terminal
            row("e", "f", "failed", Some("x")),
            row("f", "e", "pending", None),
        ];
        let r = classify_mesh_pairs(&rows);
        assert!(r.is_empty(), "clean/partial/pending pairs must not alert");
    }

    #[test]
    fn message_leads_with_asymmetric_interpretation() {
        let rows = vec![
            row("marcus", "sia", "failed", Some("timeout")),
            row("sia", "marcus", "ok", None),
        ];
        let msg = mesh_alert_message(&classify_mesh_pairs(&rows));
        assert!(msg.contains("asymmetric"));
        assert!(msg.contains("stale IP"));
        assert!(msg.contains("marcus↔sia"));
    }

    #[test]
    fn classify_both_ok_is_clean_ok() {
        assert_eq!(classify_direct_probe(true, None), ("ok".into(), None));
    }

    #[test]
    fn classify_ssh_ok_ping_failed_stays_ok_with_detail() {
        let (status, detail) = classify_direct_probe(false, None);
        assert_eq!(status, "ok");
        assert!(detail.unwrap().contains("ping failed"));
    }

    #[test]
    fn classify_ssh_failed_is_failed_and_keeps_ping_verdict() {
        let (status, detail) = classify_direct_probe(false, Some("timeout".into()));
        assert_eq!(status, "failed");
        assert_eq!(detail.as_deref(), Some("ping failed; ssh timeout"));

        let (status, detail) = classify_direct_probe(true, Some("exit 255: refused".into()));
        assert_eq!(status, "failed");
        assert_eq!(detail.as_deref(), Some("ping ok; ssh exit 255: refused"));
    }

    #[test]
    fn inactive_computer_statuses_are_not_mesh_eligible() {
        assert!(computer_status_eligible(None));
        assert!(computer_status_eligible(Some("online")));
        for status in SKIPPED_COMPUTER_STATUSES {
            assert!(!computer_status_eligible(Some(status)));
        }
    }

    #[test]
    fn retry_cap_counts_attempts_across_recreated_tasks() {
        let now = chrono::Utc::now();
        let recent = now - chrono::Duration::hours(24);
        assert!(retry_cap_reached(
            [(now, 2), (now, 2), (now, 1)].into_iter(),
            recent
        ));
        assert!(!retry_cap_reached(
            [(now, 4), (now - chrono::Duration::hours(25), 20),].into_iter(),
            recent
        ));
    }
}
