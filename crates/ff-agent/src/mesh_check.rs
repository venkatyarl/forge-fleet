//! Fleet-wide SSH mesh verification + propagation.
//! See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md §3h.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{error, warn};
use uuid::Uuid;

use crate::task_runner::{EnqueueOnceOutcome, pg_enqueue_shell_task_once};

const SKIPPED_COMPUTER_STATUSES: [&str; 3] = ["offline", "reserved", "decommissioned"];

/// Upper bound for useful fleet-wide pairwise concurrency. The scheduler below
/// also reserves both endpoint names for every in-flight edge, so any one
/// computer participates in at most one nested SSH probe at a time. With 15
/// eligible computers this permits seven simultaneous probes while preventing
/// a full N×(N-1) scan from flooding a small node's sshd.
const PAIRWISE_MESH_MAX_IN_FLIGHT: usize = 8;

/// Emergency kill switch for the autonomous leader SSH mesh-repair producer.
///
/// A missing row preserves the historical enabled behavior, and an expired
/// temporary disable restores to enabled. Any database/read error fails closed:
/// an unavailable gate authority must never create another repair queue flood.
pub const SSH_MESH_AUTO_REPAIR_ENABLED_KEY: &str = "ssh_mesh_auto_repair_enabled";
const SSH_MESH_AUTO_REPAIR_DEFAULT: bool = true;
const SSH_MESH_AUTO_REPAIR_RESTORE_ON_EXPIRY: bool = true;

const MESH_REPAIR_BACKLOG_CANCEL_SQL: &str = "UPDATE fleet_tasks
        SET status = 'cancelled',
            completed_at = COALESCE(completed_at, NOW()),
            progress_message = 'cancelled by ff fleet cleanup-mesh-repair-backlog'
      WHERE task_type = 'shell'
        AND status IN ('pending', 'dispatchable')
        AND summary LIKE 'auto-mesh-repair:%'";
const MESH_REPAIR_BACKLOG_COUNT_SQL: &str = "SELECT COUNT(*)::bigint
       FROM fleet_tasks
      WHERE task_type = 'shell'
        AND status IN ('pending', 'dispatchable')
        AND summary LIKE 'auto-mesh-repair:%'";

fn resolve_ssh_mesh_auto_repair_gate<E>(result: Result<bool, E>) -> bool
where
    E: std::fmt::Display,
{
    match result {
        Ok(enabled) => enabled,
        Err(error) => {
            warn!(
                key = SSH_MESH_AUTO_REPAIR_ENABLED_KEY,
                %error,
                "SSH mesh auto-repair gate read failed; refusing new tasks"
            );
            false
        }
    }
}

/// Read the authoritative autonomous mesh-repair gate.
pub async fn ssh_mesh_auto_repair_enabled(pool: &PgPool) -> bool {
    resolve_ssh_mesh_auto_repair_gate(
        ff_db::pg_read_safety_gate(
            pool,
            SSH_MESH_AUTO_REPAIR_ENABLED_KEY,
            SSH_MESH_AUTO_REPAIR_DEFAULT,
            SSH_MESH_AUTO_REPAIR_RESTORE_ON_EXPIRY,
        )
        .await,
    )
}

fn mesh_auto_repair_enqueue_key(src: &str, dst: &str) -> String {
    format!("ssh-mesh-auto-repair:{src}:{dst}")
}

/// Enqueue at most one active autonomous repair for a directed mesh edge.
///
/// The stable per-edge key is protected by the fleet task unique index and a
/// transaction-scoped advisory lock in `pg_enqueue_shell_task_once`, so daemon
/// restarts and leadership races cannot create duplicate active rows.
pub async fn enqueue_ssh_mesh_auto_repair(
    pool: &PgPool,
    leader_name: &str,
    src: &str,
    dst: &str,
) -> Result<EnqueueOnceOutcome, sqlx::Error> {
    let outcome = enqueue_ssh_mesh_auto_repair_scoped(
        pool,
        leader_name,
        src,
        dst,
        &MeshExclusions::default(),
    )
    .await?;
    Ok(outcome.expect("an empty exclusion set always permits the edge"))
}

/// Scoped variant used by operator-driven runs. Excluded edges return `None`
/// before a task key, command, or database row is produced.
pub async fn enqueue_ssh_mesh_auto_repair_scoped(
    pool: &PgPool,
    leader_name: &str,
    src: &str,
    dst: &str,
    exclusions: &MeshExclusions,
) -> Result<Option<EnqueueOnceOutcome>, sqlx::Error> {
    if !exclusions.allows_edge(src, dst) {
        return Ok(None);
    }
    let summary = format!("auto-mesh-repair: {src} -> {dst}");
    let command = format!("ff fleet ssh-mesh-check --node {dst} --repair --yes 2>&1 | tail -10");
    pg_enqueue_shell_task_once(
        pool,
        &mesh_auto_repair_enqueue_key(src, dst),
        &summary,
        &command,
        &["ff".to_string()],
        Some(leader_name),
        None,
        50,
        None,
    )
    .await
    .map(Some)
}

/// Result of the narrowly-scoped autonomous mesh-repair queue cleanup verb.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MeshRepairBacklogCancellation {
    /// Number of pending/dispatchable auto-repair tasks matched by the operation.
    pub eligible: u64,
    /// Number actually moved to `cancelled` (`0` for a dry run).
    pub cancelled: u64,
    pub applied: bool,
}

/// Preview or cancel only unstarted `auto-mesh-repair:` shell tasks.
///
/// Apply is a single guarded update. A row that races to `running` before the
/// update locks it is re-checked and left untouched; running and terminal rows
/// are never eligible.
pub async fn cancel_mesh_auto_repair_backlog(
    pool: &PgPool,
    apply: bool,
) -> Result<MeshRepairBacklogCancellation, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if apply {
        let cancelled = sqlx::query(MESH_REPAIR_BACKLOG_CANCEL_SQL)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(MeshRepairBacklogCancellation {
            eligible: cancelled,
            cancelled,
            applied: true,
        })
    } else {
        let eligible: i64 = sqlx::query_scalar(MESH_REPAIR_BACKLOG_COUNT_SQL)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(MeshRepairBacklogCancellation {
            eligible: u64::try_from(eligible).unwrap_or(0),
            cancelled: 0,
            applied: false,
        })
    }
}

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
    exclusions: &MeshExclusions,
) -> Result<(), String> {
    let names: Vec<&str> = nodes
        .iter()
        .filter(|node| !mesh_eligible(node) && !exclusions.contains(&node.name))
        .map(|node| node.name.as_str())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let excluded_names: Vec<&str> = exclusions.iter().map(String::as_str).collect();
    sqlx::query(
        "UPDATE fleet_mesh_status
            SET status = 'skipped', last_checked = NOW(),
                last_error = 'endpoint computer is offline, reserved, or decommissioned'
          WHERE (src_node = ANY($1) OR dst_node = ANY($1))
            AND NOT (src_node = ANY($2) OR dst_node = ANY($2))",
    )
    .bind(&names)
    .bind(&excluded_names)
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
    pub ping_ok: Option<bool>,
    pub ssh_ok: bool,
}

#[derive(Debug, Clone)]
pub struct MeshMatrix {
    pub cells: Vec<MeshCell>,
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct MeshProbe {
    src: String,
    src_user: String,
    src_ip: String,
    dst: String,
    dst_user: String,
    dst_ip: String,
}

/// Canonical runtime-only mesh endpoints that an operator explicitly excluded.
/// The set is never persisted; callers must pass it through the current run.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MeshExclusions {
    names: BTreeSet<String>,
}

impl MeshExclusions {
    #[cfg(test)]
    pub(crate) fn from_canonical_names(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }

    pub fn contains(&self, node: &str) -> bool {
        self.names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(node))
    }

    pub fn allows_edge(&self, src: &str, dst: &str) -> bool {
        !self.contains(src) && !self.contains(dst)
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = &String> {
        self.names.iter()
    }
}

/// Canonical selected-node and endpoint-exclusion scope for one mesh command.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MeshCheckScope {
    only_node: Option<String>,
    exclusions: MeshExclusions,
}

impl MeshCheckScope {
    pub fn only_node(&self) -> Option<&str> {
        self.only_node.as_deref()
    }

    pub fn exclusions(&self) -> &MeshExclusions {
        &self.exclusions
    }

    pub fn edge_in_scope(&self, src: &str, dst: &str) -> bool {
        self.exclusions.allows_edge(src, dst)
            && self
                .only_node
                .as_deref()
                .is_none_or(|node| src.eq_ignore_ascii_case(node) || dst.eq_ignore_ascii_case(node))
    }

    fn destination_in_scope(&self, src: &str, dst: &str) -> bool {
        self.exclusions.allows_edge(src, dst)
            && self
                .only_node
                .as_deref()
                .is_none_or(|node| dst.eq_ignore_ascii_case(node))
    }
}

fn resolve_mesh_check_scope_from_names(
    available_names: impl IntoIterator<Item = String>,
    only_node: Option<&str>,
    requested_exclusions: &[String],
) -> Result<MeshCheckScope, String> {
    let available: Vec<String> = available_names.into_iter().collect();
    let resolve = |requested: &str| {
        let requested = requested.trim();
        available
            .iter()
            .find(|name| name.eq_ignore_ascii_case(requested))
            .cloned()
    };

    let only_node = only_node
        .map(|requested| {
            resolve(requested).ok_or_else(|| format!("unknown selected node '{requested}'"))
        })
        .transpose()?;

    let mut names = BTreeSet::new();
    for requested in requested_exclusions {
        let canonical = resolve(requested)
            .ok_or_else(|| format!("unknown excluded node '{}'", requested.trim()))?;
        names.insert(canonical);
    }
    if let Some(selected) = &only_node
        && names.contains(selected)
    {
        return Err(format!(
            "selected node '{selected}' cannot also be excluded"
        ));
    }

    Ok(MeshCheckScope {
        only_node,
        exclusions: MeshExclusions { names },
    })
}

/// Resolve operator-supplied names against the authoritative fleet registry.
pub async fn resolve_mesh_check_scope(
    pool: &PgPool,
    only_node: Option<&str>,
    requested_exclusions: &[String],
) -> Result<MeshCheckScope, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    resolve_mesh_check_scope_from_names(
        nodes.into_iter().map(|node| node.name),
        only_node,
        requested_exclusions,
    )
}

pub async fn pairwise_ssh_check(pool: &PgPool) -> Result<MeshMatrix, String> {
    pairwise_ssh_check_scoped(pool, &MeshCheckScope::default()).await
}

pub async fn pairwise_ssh_check_node(pool: &PgPool, node: &str) -> Result<MeshMatrix, String> {
    let scope = resolve_mesh_check_scope(pool, Some(node), &[]).await?;
    pairwise_ssh_check_scoped(pool, &scope).await
}

pub async fn pairwise_ssh_check_scoped(
    pool: &PgPool,
    scope: &MeshCheckScope,
) -> Result<MeshMatrix, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes, scope.exclusions()).await?;
    let matrix = pairwise_ssh_check_inner(pool, &nodes, scope).await?;
    let _ = fire_mesh_alert_scoped(pool, scope.exclusions()).await;
    Ok(matrix)
}

fn mesh_probe_plan(nodes: &[ff_db::FleetNodeRow], scope: &MeshCheckScope) -> Vec<MeshProbe> {
    let mut probes = Vec::new();
    for src in nodes {
        for dst in nodes {
            if src.name == dst.name
                || !mesh_eligible(src)
                || !mesh_eligible(dst)
                || !scope.edge_in_scope(&src.name, &dst.name)
            {
                continue;
            }
            probes.push(MeshProbe {
                src: src.name.clone(),
                src_user: src.ssh_user.clone(),
                src_ip: src.ip.clone(),
                dst: dst.name.clone(),
                dst_user: dst.ssh_user.clone(),
                dst_ip: dst.ip.clone(),
            });
        }
    }
    probes.sort_by(|a, b| (&a.src, &a.dst).cmp(&(&b.src, &b.dst)));
    probes
}

/// Execute directed mesh probes with both endpoints reserved for the lifetime
/// of each probe. This is stricter than a destination-only cap: because the
/// outer SSH hop also lands on `src`, a small computer is protected whether it
/// appears as source or destination. Independent, node-disjoint edges still
/// run concurrently up to `max_in_flight`.
async fn run_bounded_mesh_probes<F, Fut>(
    probes: Vec<MeshProbe>,
    max_in_flight: usize,
    run_probe: F,
) -> Vec<MeshCell>
where
    F: Fn(MeshProbe) -> Fut,
    Fut: Future<Output = MeshCell>,
{
    use futures::stream::{FuturesUnordered, StreamExt};

    let mut pending: VecDeque<MeshProbe> = probes.into();
    let mut active_nodes = BTreeSet::new();
    let mut in_flight = FuturesUnordered::new();
    let mut cells = Vec::with_capacity(pending.len());
    let max_in_flight = max_in_flight.max(1);

    while !pending.is_empty() || !in_flight.is_empty() {
        while in_flight.len() < max_in_flight {
            let Some(index) = pending.iter().position(|probe| {
                !active_nodes.contains(&probe.src) && !active_nodes.contains(&probe.dst)
            }) else {
                break;
            };
            let probe = pending
                .remove(index)
                .expect("selected mesh probe index remains valid");
            let src = probe.src.clone();
            let dst = probe.dst.clone();
            active_nodes.insert(src.clone());
            active_nodes.insert(dst.clone());
            let future = run_probe(probe);
            in_flight.push(async move { (src, dst, future.await) });
        }

        let Some((src, dst, cell)) = in_flight.next().await else {
            debug_assert!(pending.is_empty());
            break;
        };
        active_nodes.remove(&src);
        active_nodes.remove(&dst);
        cells.push(cell);
    }

    cells.sort_by(|a, b| (&a.src, &a.dst).cmp(&(&b.src, &b.dst)));
    cells
}

async fn pairwise_ssh_check_inner(
    pool: &PgPool,
    nodes: &[ff_db::FleetNodeRow],
    scope: &MeshCheckScope,
) -> Result<MeshMatrix, String> {
    let probes = mesh_probe_plan(nodes, scope);
    let cells = run_bounded_mesh_probes(probes, PAIRWISE_MESH_MAX_IN_FLIGHT, |probe| async move {
        let cell = probe_pair(
            probe.src,
            probe.src_user,
            probe.src_ip,
            probe.dst,
            probe.dst_user,
            probe.dst_ip,
        )
        .await;
        let _ = ff_db::pg_upsert_mesh_probe(
            pool,
            &cell.src,
            &cell.dst,
            &cell.status,
            cell.last_error.as_deref(),
            cell.ping_ok,
            Some(cell.ssh_ok),
        )
        .await;
        cell
    })
    .await;

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
        "ping -c 1 {dst_ip} >/dev/null 2>&1; p=$?; \
         ssh {ssh_bypass} -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {dst_user}@{dst_ip} true; s=$?; printf '__FF_MESH__%s:%s\\n' \"$p\" \"$s\"; exit \"$s\""
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
            ping_ok: parse_remote_probe_marker(&out.stdout).map(|(ping, _)| ping),
            ssh_ok: true,
            src,
            dst,
            status: "ok".into(),
            last_error: None,
        },
        Ok(Ok(out)) => MeshCell {
            ping_ok: parse_remote_probe_marker(&out.stdout).map(|(ping, _)| ping),
            ssh_ok: false,
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
            ping_ok: None,
            ssh_ok: false,
            src,
            dst,
            status: "failed".into(),
            last_error: Some(format!("spawn: {e}")),
        },
        Err(_) => MeshCell {
            ping_ok: None,
            ssh_ok: false,
            src,
            dst,
            status: "failed".into(),
            last_error: Some("timeout".into()),
        },
    }
}

fn parse_remote_probe_marker(stdout: &[u8]) -> Option<(bool, bool)> {
    let text = String::from_utf8_lossy(stdout);
    let marker = text
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("__FF_MESH__"))?;
    let (ping, ssh) = marker.split_once(':')?;
    Some((ping == "0", ssh == "0"))
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
    let scope = resolve_mesh_check_scope(pool, only_node, &[]).await?;
    local_reach_check_scoped(pool, &scope).await
}

pub async fn local_reach_check_scoped(
    pool: &PgPool,
    scope: &MeshCheckScope,
) -> Result<Vec<LocalProbe>, String> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let me = crate::fleet_info::resolve_this_worker_name().await;
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes, scope.exclusions()).await?;

    let mut futs = FuturesUnordered::new();
    let mut probes = Vec::new();
    for n in nodes.iter().filter(|n| n.name != me && mesh_eligible(n)) {
        if !scope.destination_in_scope(&me, &n.name) {
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
            let _ = ff_db::pg_upsert_mesh_probe(
                pool,
                &p.src,
                &p.dst,
                &p.status,
                p.detail.as_deref(),
                Some(p.ping_ok),
                Some(p.ssh_ok),
            )
            .await;
            probes.push(p);
        }
    }
    while let Some(p) = futs.next().await {
        let _ = ff_db::pg_upsert_mesh_probe(
            pool,
            &p.src,
            &p.dst,
            &p.status,
            p.detail.as_deref(),
            Some(p.ping_ok),
            Some(p.ssh_ok),
        )
        .await;
        probes.push(p);
    }
    let _ = fire_mesh_alert_scoped(pool, scope.exclusions()).await;
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

/// Alert policy seeded by migration V179.
const MESH_ALERT_POLICY: &str = "ssh_mesh_degraded";
const MESH_ALERT_RECENCY_HOURS: i64 = 24;

#[derive(Debug, Default)]
struct MeshAlertSnapshot {
    failed_edges: Vec<(String, String, Option<String>)>,
    asymmetric: Vec<(String, String, String, String)>,
}

type MeshAlertRow = (
    String,
    String,
    String,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

async fn load_mesh_alert_snapshot(
    pg: &PgPool,
    exclusions: &MeshExclusions,
) -> Result<MeshAlertSnapshot, String> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(MESH_ALERT_RECENCY_HOURS);
    let rows: Vec<MeshAlertRow> = sqlx::query_as(
        "SELECT src_node, dst_node, status, last_error, last_checked
         FROM fleet_mesh_status
         ORDER BY src_node, dst_node",
    )
    .fetch_all(pg)
    .await
    .map_err(|e| format!("load mesh status: {e}"))?;

    Ok(mesh_alert_snapshot_from_rows(rows, cutoff, exclusions))
}

fn mesh_alert_snapshot_from_rows(
    rows: impl IntoIterator<Item = MeshAlertRow>,
    cutoff: chrono::DateTime<chrono::Utc>,
    exclusions: &MeshExclusions,
) -> MeshAlertSnapshot {
    let mut directed: BTreeMap<(String, String), (String, Option<String>)> = BTreeMap::new();
    for (src, dst, status, last_error, last_checked) in rows {
        if !exclusions.allows_edge(&src, &dst) || last_checked.map(|t| t < cutoff).unwrap_or(true) {
            continue;
        }
        directed.insert((src, dst), (status, last_error));
    }

    let mut snapshot = MeshAlertSnapshot::default();
    for ((src, dst), (status, last_error)) in &directed {
        if status == "failed" {
            snapshot
                .failed_edges
                .push((src.clone(), dst.clone(), last_error.clone()));
        }
    }

    let mut names: Vec<String> = directed.keys().map(|(a, _)| a.clone()).collect();
    names.sort();
    names.dedup();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let a = &names[i];
            let b = &names[j];
            let Some((ab_status, _)) = directed.get(&(a.clone(), b.clone())) else {
                continue;
            };
            let Some((ba_status, _)) = directed.get(&(b.clone(), a.clone())) else {
                continue;
            };
            if ab_status != ba_status {
                snapshot.asymmetric.push((
                    a.clone(),
                    b.clone(),
                    ab_status.clone(),
                    ba_status.clone(),
                ));
            }
        }
    }

    snapshot
}

/// Fire the `ssh_mesh_degraded` imperative alert if the recent mesh snapshot
/// contains any failed directed pairs or asymmetric pairs. Called automatically
/// after full pairwise checks and local reachability checks so both scheduled
/// ticks and on-demand `ff fleet ssh-mesh-check` alert on problems.
pub async fn fire_mesh_alert(pg: &PgPool) -> Result<(), String> {
    fire_mesh_alert_scoped(pg, &MeshExclusions::default()).await
}

pub async fn fire_mesh_alert_scoped(
    pg: &PgPool,
    exclusions: &MeshExclusions,
) -> Result<(), String> {
    let policy: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
    )
    .bind(MESH_ALERT_POLICY)
    .fetch_optional(pg)
    .await
    .map_err(|e| format!("load {MESH_ALERT_POLICY} policy: {e}"))?;

    let Some((policy_id, severity, channel)) = policy else {
        error!(
            policy = MESH_ALERT_POLICY,
            "ssh-mesh: alert policy missing or disabled"
        );
        return Ok(());
    };

    let snapshot = load_mesh_alert_snapshot(pg, exclusions).await?;
    let total = snapshot.failed_edges.len() + snapshot.asymmetric.len();
    if total == 0 {
        return Ok(());
    }

    let mut parts = Vec::new();
    if !snapshot.failed_edges.is_empty() {
        let summary = snapshot
            .failed_edges
            .iter()
            .take(12)
            .map(|(a, b, e)| {
                let extra = e.as_ref().map(|x| format!(" ({x})")).unwrap_or_default();
                format!("{a}->{b}{extra}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ellipsis = if snapshot.failed_edges.len() > 12 {
            ", ..."
        } else {
            ""
        };
        parts.push(format!("failed: {summary}{ellipsis}"));
    }
    if !snapshot.asymmetric.is_empty() {
        let summary = snapshot
            .asymmetric
            .iter()
            .take(12)
            .map(|(a, b, ab, ba)| format!("{a}->{b}={ab}, {b}->{a}={ba}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ellipsis = if snapshot.asymmetric.len() > 12 {
            ", ..."
        } else {
            ""
        };
        parts.push(format!("asymmetric: {summary}{ellipsis}"));
    }

    let message = format!(
        "SSH mesh degraded: {} unhealthy pair(s). {}",
        total,
        parts.join("; ")
    );

    let channel_result =
        crate::alert_evaluator::dispatch_alert(pg, &channel, &severity, &message).await;

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO alert_events
            (policy_id, computer_id, value, value_text, message, channel_result)
        VALUES ($1, NULL, $2, NULL, $3, $4)
        "#,
    )
    .bind(policy_id)
    .bind(total as f64)
    .bind(&message)
    .bind(&channel_result)
    .execute(pg)
    .await
    {
        error!(error = %e, "ssh-mesh: failed to record alert_event");
    }

    warn!(
        total,
        failed = snapshot.failed_edges.len(),
        asymmetric = snapshot.asymmetric.len(),
        channel = %channel,
        channel_result = %channel_result,
        "ssh-mesh: degraded-pair alert fired"
    );

    Ok(())
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
    mark_ineligible_pairs_skipped(pool, &nodes, &MeshExclusions::default()).await?;
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
    mark_ineligible_pairs_skipped(pool, &nodes, &MeshExclusions::default()).await?;
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
            ping_ok: None,
            ssh_ok: false,
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
    let _ = ff_db::pg_upsert_mesh_probe(
        pool,
        &cell.src,
        &cell.dst,
        &cell.status,
        cell.last_error.as_deref(),
        cell.ping_ok,
        Some(cell.ssh_ok),
    )
    .await;
    Ok(cell)
}

/// For every `fleet_mesh_status` row in status='failed' whose last_checked is
/// older than 10 minutes, enqueue a `mesh_retry` deferred task — de-duplicated
/// against any active retry for the same (src,dst) pair. Capped at 5 attempts
/// per 24h across task IDs so a completed task cannot reset the retry budget.
pub async fn enqueue_retries(pool: &PgPool) -> Result<usize, String> {
    enqueue_retries_scoped(pool, &MeshCheckScope::default()).await
}

pub async fn enqueue_retries_scoped(
    pool: &PgPool,
    scope: &MeshCheckScope,
) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
    let retry_window = chrono::Utc::now() - chrono::Duration::hours(24);
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    mark_ineligible_pairs_skipped(pool, &nodes, scope.exclusions()).await?;
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
                && scope.edge_in_scope(&r.src_node, &r.dst_node)
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
            Some("vinny"),
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
    refresh_stale_scoped(pool, max_age, &MeshCheckScope::default()).await
}

pub async fn refresh_stale_scoped(
    pool: &PgPool,
    max_age: chrono::Duration,
    scope: &MeshCheckScope,
) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - max_age;
    let all = ff_db::pg_list_mesh_status(pool, None)
        .await
        .map_err(|e| format!("pg_list_mesh_status: {e}"))?;
    let stale: HashSet<(String, String)> = all
        .iter()
        .filter(|r| {
            scope.edge_in_scope(&r.src_node, &r.dst_node)
                && r.last_checked.map(|t| t < cutoff).unwrap_or(true)
        })
        .map(|r| (r.src_node.clone(), r.dst_node.clone()))
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let _ = pairwise_ssh_check_scoped(pool, scope).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_repair_gate_defaults_and_ttl_restore_enabled_but_errors_fail_closed() {
        assert!(SSH_MESH_AUTO_REPAIR_DEFAULT);
        assert!(SSH_MESH_AUTO_REPAIR_RESTORE_ON_EXPIRY);
        assert!(resolve_ssh_mesh_auto_repair_gate(
            Ok::<bool, anyhow::Error>(true)
        ));
        assert!(!resolve_ssh_mesh_auto_repair_gate(
            Ok::<bool, anyhow::Error>(false)
        ));
        assert!(!resolve_ssh_mesh_auto_repair_gate(Err(anyhow::anyhow!(
            "gate database unavailable"
        ))));
    }

    #[test]
    fn enqueue_once_key_is_stable_and_scoped_to_the_directed_edge() {
        assert_eq!(
            mesh_auto_repair_enqueue_key("shakira", "beyonce"),
            "ssh-mesh-auto-repair:shakira:beyonce"
        );
        assert_ne!(
            mesh_auto_repair_enqueue_key("shakira", "beyonce"),
            mesh_auto_repair_enqueue_key("beyonce", "shakira")
        );
    }

    #[test]
    fn backlog_cleanup_sql_is_narrow_and_never_names_running_or_terminal_statuses() {
        for sql in [
            MESH_REPAIR_BACKLOG_COUNT_SQL,
            MESH_REPAIR_BACKLOG_CANCEL_SQL,
        ] {
            assert!(sql.contains("task_type = 'shell'"));
            assert!(sql.contains("status IN ('pending', 'dispatchable')"));
            assert!(sql.contains("summary LIKE 'auto-mesh-repair:%'"));
            assert!(!sql.contains("status IN ('running'"));
            assert!(!sql.contains("status IN ('completed'"));
        }
    }

    #[test]
    fn parses_remote_ping_and_ssh_verdicts() {
        assert_eq!(
            parse_remote_probe_marker(b"__FF_MESH__0:0\n"),
            Some((true, true))
        );
        assert_eq!(
            parse_remote_probe_marker(b"noise\n__FF_MESH__1:0\n"),
            Some((false, true))
        );
        assert_eq!(parse_remote_probe_marker(b"no marker"), None);
    }

    #[tokio::test]
    async fn pairwise_scheduler_serializes_each_endpoint_without_losing_parallelism() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        let names = ["a", "b", "c", "d", "e", "f"];
        let mut probes = Vec::new();
        for src in names {
            for dst in names {
                if src == dst {
                    continue;
                }
                probes.push(MeshProbe {
                    src: src.into(),
                    src_user: "user".into(),
                    src_ip: "127.0.0.1".into(),
                    dst: dst.into(),
                    dst_user: "user".into(),
                    dst_ip: "127.0.0.1".into(),
                });
            }
        }
        probes.sort_by(|a, b| (&a.src, &a.dst).cmp(&(&b.src, &b.dst)));

        let active_by_node = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
        let active_total = Arc::new(AtomicUsize::new(0));
        let max_total = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        // Six nodes permit a three-edge matching. Holding the first wave at a
        // barrier proves independent edges overlap instead of accidentally
        // degenerating to a fleet-wide serial scan.
        let first_wave = Arc::new(tokio::sync::Barrier::new(3));

        let run = run_bounded_mesh_probes(probes, PAIRWISE_MESH_MAX_IN_FLIGHT, |probe| {
            let active_by_node = Arc::clone(&active_by_node);
            let active_total = Arc::clone(&active_total);
            let max_total = Arc::clone(&max_total);
            let started = Arc::clone(&started);
            let first_wave = Arc::clone(&first_wave);
            async move {
                {
                    let mut active = active_by_node.lock().expect("endpoint counter lock");
                    for endpoint in [&probe.src, &probe.dst] {
                        let count = active.entry(endpoint.clone()).or_default();
                        assert_eq!(
                            *count, 0,
                            "endpoint {endpoint} participated in concurrent mesh probes"
                        );
                        *count += 1;
                    }
                }
                let current = active_total.fetch_add(1, Ordering::SeqCst) + 1;
                max_total.fetch_max(current, Ordering::SeqCst);
                if started.fetch_add(1, Ordering::SeqCst) < 3 {
                    first_wave.wait().await;
                }
                tokio::task::yield_now().await;
                active_total.fetch_sub(1, Ordering::SeqCst);
                {
                    let mut active = active_by_node.lock().expect("endpoint counter lock");
                    for endpoint in [&probe.src, &probe.dst] {
                        *active.get_mut(endpoint).expect("active endpoint") -= 1;
                    }
                }
                MeshCell {
                    src: probe.src,
                    dst: probe.dst,
                    status: "ok".into(),
                    last_error: None,
                    ping_ok: Some(true),
                    ssh_ok: true,
                }
            }
        });
        let cells = timeout(Duration::from_secs(2), run)
            .await
            .expect("bounded scheduler deadlocked");

        assert_eq!(cells.len(), names.len() * (names.len() - 1));
        assert_eq!(
            max_total.load(Ordering::SeqCst),
            3,
            "six endpoints should retain three useful concurrent probes"
        );
        assert!(
            cells
                .windows(2)
                .all(|pair| { (&pair[0].src, &pair[0].dst) <= (&pair[1].src, &pair[1].dst) })
        );
        assert!(
            active_by_node
                .lock()
                .expect("endpoint counter lock")
                .values()
                .all(|count| *count == 0)
        );
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

    #[test]
    fn mesh_scope_canonicalizes_deduplicates_and_rejects_invalid_names() {
        let available = || {
            ["Logan", "Vinny", "Sia", "Beyonce"]
                .into_iter()
                .map(str::to_string)
        };
        let scope = resolve_mesh_check_scope_from_names(
            available(),
            Some("logan"),
            &[" VINNY ".into(), "vinny".into()],
        )
        .expect("valid scope");

        assert_eq!(scope.only_node(), Some("Logan"));
        assert_eq!(scope.exclusions.iter().collect::<Vec<_>>(), [&"Vinny"]);
        assert!(scope.exclusions.contains("VINNY"));
        assert!(!scope.edge_in_scope("logan", "VINNY"));
        assert!(scope.edge_in_scope("LOGAN", "SIA"));
        assert_eq!(
            resolve_mesh_check_scope_from_names(available(), None, &["unknown".into()])
                .unwrap_err(),
            "unknown excluded node 'unknown'"
        );
        assert_eq!(
            resolve_mesh_check_scope_from_names(available(), Some("Vinny"), &["vinny".into()])
                .unwrap_err(),
            "selected node 'Vinny' cannot also be excluded"
        );
    }

    #[test]
    fn exclusions_remove_both_edge_directions_without_changing_legacy_scope() {
        let names = ["Logan", "Vinny", "Sia", "Beyonce"];
        let selected = resolve_mesh_check_scope_from_names(
            names.into_iter().map(str::to_string),
            Some("Logan"),
            &["Vinny".into()],
        )
        .expect("valid scope");
        let legacy = resolve_mesh_check_scope_from_names(
            names.into_iter().map(str::to_string),
            Some("Logan"),
            &[],
        )
        .expect("legacy scope");

        for src in names {
            for dst in names {
                if src == dst {
                    continue;
                }
                assert_eq!(
                    legacy.edge_in_scope(src, dst),
                    src == "Logan" || dst == "Logan",
                    "empty exclusions must preserve the selected-node predicate for {src}->{dst}"
                );
                assert_eq!(
                    selected.edge_in_scope(src, dst),
                    (src == "Logan" || dst == "Logan") && src != "Vinny" && dst != "Vinny",
                    "excluded endpoint leaked into {src}->{dst}"
                );
            }
        }
    }

    #[test]
    fn exclusions_guard_retry_repair_and_direct_probe_predicates() {
        let scope = resolve_mesh_check_scope_from_names(
            ["Logan", "Vinny", "Sia"].into_iter().map(str::to_string),
            None,
            &["Vinny".into()],
        )
        .expect("valid scope");

        assert!(!scope.edge_in_scope("Logan", "Vinny"));
        assert!(!scope.edge_in_scope("Vinny", "Logan"));
        assert!(!scope.destination_in_scope("Logan", "Vinny"));
        assert!(!scope.destination_in_scope("Vinny", "Logan"));
        assert!(scope.edge_in_scope("Logan", "Sia"));
        assert!(scope.destination_in_scope("Logan", "Sia"));
    }

    #[tokio::test]
    async fn excluded_auto_repair_returns_without_opening_a_database_connection() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgresql://invalid:invalid@127.0.0.1:1/invalid")
            .expect("lazy pool");
        let exclusions = MeshExclusions::from_canonical_names(["Vinny".into()]);

        let result =
            enqueue_ssh_mesh_auto_repair_scoped(&pool, "Logan", "Logan", "Vinny", &exclusions)
                .await
                .expect("excluded repair should be a no-op");
        assert!(result.is_none());
    }

    #[test]
    fn alert_snapshot_ignores_excluded_failures_and_asymmetry_only_when_explicit() {
        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::hours(24);
        let rows = vec![
            (
                "Logan".into(),
                "Vinny".into(),
                "failed".into(),
                Some("down".into()),
                Some(now),
            ),
            ("Vinny".into(), "Logan".into(), "ok".into(), None, Some(now)),
            ("Logan".into(), "Sia".into(), "ok".into(), None, Some(now)),
            ("Sia".into(), "Logan".into(), "ok".into(), None, Some(now)),
        ];

        let legacy =
            mesh_alert_snapshot_from_rows(rows.clone(), cutoff, &MeshExclusions::default());
        assert_eq!(legacy.failed_edges.len(), 1);
        assert_eq!(legacy.asymmetric.len(), 1);

        let excluding_vinny = MeshExclusions::from_canonical_names(["Vinny".into()]);
        let scoped = mesh_alert_snapshot_from_rows(rows, cutoff, &excluding_vinny);
        assert!(scoped.failed_edges.is_empty());
        assert!(scoped.asymmetric.is_empty());

        let still_failed = mesh_alert_snapshot_from_rows(
            vec![(
                "Logan".into(),
                "Beyonce".into(),
                "failed".into(),
                Some("refused".into()),
                Some(now),
            )],
            cutoff,
            &excluding_vinny,
        );
        assert_eq!(still_failed.failed_edges.len(), 1);
    }
}
