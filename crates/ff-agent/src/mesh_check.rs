//! Fleet-wide SSH mesh verification + propagation.
//! See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md §3h.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::process::{Output, Stdio};
use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
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
const PAIRWISE_MESH_TRANSIENT_COOLDOWN: Duration = Duration::from_secs(30);
// The legacy daemon runs this tick inline and declares itself wedged after five
// minutes. Leave enough room for child cancellation and bounded persistence.
const PAIRWISE_MESH_SCAN_DEADLINE: Duration = Duration::from_secs(3 * 60);
const PAIRWISE_MESH_MAX_TRANSIENT_RETRIES: u8 = 1;
const MESH_SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const MESH_PERSIST_MAX_IN_FLIGHT: usize = 8;
const MESH_PERSIST_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MESH_PERSIST_GLOBAL_TIMEOUT: Duration = Duration::from_secs(45);
const MESH_DB_STEP_TIMEOUT: Duration = Duration::from_secs(10);
const FULL_MESH_OPERATION_DEADLINE: Duration = Duration::from_secs(260);
const MESH_OPERATION_CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const MESH_SCAN_DEADLINE_ERROR: &str = "mesh scan deadline exceeded before probe completed";
const MESH_OPERATION_CANCELLED_ERROR: &str = "mesh operation cancelled";
const MESH_SCAN_BUSY_ERROR: &str = "an SSH mesh scan is already in progress";
const MESH_SCAN_FENCE_SQL: &str = "SELECT pg_try_advisory_xact_lock($1)";
// Stable, repository-owned namespace for the single fleet-wide scan fence.
const MESH_SCAN_FENCE_KEY: i64 = 0x4646_4d45_5348_0001;

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

#[derive(Debug, Clone)]
struct ScheduledMeshProbe {
    probe: MeshProbe,
    transient_retries: u8,
}

#[derive(Debug)]
struct MeshScheduleResult {
    cells: Vec<MeshCell>,
    /// Only final, actually-observed logical results belong in the database.
    /// Physical retry failures and deadline-generated cells are diagnostics,
    /// not authoritative observations.
    persistable_edges: BTreeSet<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
struct MeshSchedulePolicy {
    max_in_flight: usize,
    transient_cooldown: Duration,
    scan_deadline: Duration,
    max_transient_retries: u8,
}

const PAIRWISE_MESH_SCHEDULE_POLICY: MeshSchedulePolicy = MeshSchedulePolicy {
    max_in_flight: PAIRWISE_MESH_MAX_IN_FLIGHT,
    transient_cooldown: PAIRWISE_MESH_TRANSIENT_COOLDOWN,
    scan_deadline: PAIRWISE_MESH_SCAN_DEADLINE,
    max_transient_retries: PAIRWISE_MESH_MAX_TRANSIENT_RETRIES,
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TransientFailureScope {
    Source,
    Destination,
    Both,
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

/// Hold a transaction-scoped advisory lock for the whole scan. A transaction
/// lock is important here: dropping a cancelled task returns the pooled
/// connection only after SQLx rolls the transaction back, so no session-level
/// lock can leak into an unrelated borrower.
async fn bounded_mesh_step<T, F, E>(
    cancellation: &CancellationToken,
    label: &str,
    future: F,
) -> Result<T, String>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(MESH_OPERATION_CANCELLED_ERROR.into()),
        result = timeout(MESH_DB_STEP_TIMEOUT, future) => result
            .map_err(|_| format!("{label} timed out"))?
            .map_err(|error| format!("{label}: {error}")),
    }
}

async fn acquire_mesh_scan_fence<'pool>(
    pool: &'pool PgPool,
    cancellation: &CancellationToken,
) -> Result<Transaction<'pool, Postgres>, String> {
    let mut tx = bounded_mesh_step(cancellation, "begin SSH mesh scan fence", pool.begin()).await?;
    let acquired: bool = bounded_mesh_step(
        cancellation,
        "acquire SSH mesh scan fence",
        sqlx::query_scalar(MESH_SCAN_FENCE_SQL)
            .bind(MESH_SCAN_FENCE_KEY)
            .fetch_one(&mut *tx),
    )
    .await?;
    if !acquired {
        timeout(MESH_DB_STEP_TIMEOUT, tx.rollback())
            .await
            .map_err(|_| "release busy SSH mesh scan fence timed out".to_string())?
            .map_err(|error| format!("release busy SSH mesh scan fence: {error}"))?;
        return Err(MESH_SCAN_BUSY_ERROR.into());
    }
    Ok(tx)
}

async fn finish_mesh_scan<T>(
    fence: Transaction<'_, Postgres>,
    result: Result<T, String>,
) -> Result<T, String> {
    let release = timeout(MESH_DB_STEP_TIMEOUT, fence.rollback())
        .await
        .map_err(|_| "release SSH mesh scan fence timed out".to_string())?
        .map_err(|error| format!("release SSH mesh scan fence: {error}"));
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(operation), Err(release)) => Err(format!("{operation}; {release}")),
    }
}

struct CancelMeshOperationOnDrop {
    cancellation: CancellationToken,
    armed: bool,
}

impl CancelMeshOperationOnDrop {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelMeshOperationOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

/// Run an operation in an owned task so dropping a CLI/daemon caller cancels
/// rather than drops the process-owning future. The task remains responsible
/// for draining probes and releasing its fence before it exits.
async fn run_owned_mesh_operation<T, F, Fut>(label: &'static str, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = Result<T, String>> + Send + 'static,
{
    let cancellation = CancellationToken::new();
    let mut cancel_on_drop = CancelMeshOperationOnDrop::new(cancellation.clone());
    let mut task = tokio::spawn(operation(cancellation.clone()));
    let joined = match timeout(FULL_MESH_OPERATION_DEADLINE, &mut task).await {
        Ok(joined) => joined,
        Err(_) => {
            cancellation.cancel();
            match timeout(MESH_OPERATION_CLEANUP_TIMEOUT, &mut task).await {
                Ok(joined) => joined,
                Err(_) => {
                    // Return to the daemon before its 300-second watchdog, but
                    // detach (do not abort) the process-owning task. It keeps
                    // the cancellation token and must finish its direct-child
                    // wait/reap even if kernel cleanup takes unusually long.
                    // Aborting here would turn a bounded error into a leaked
                    // child/process group.
                    drop(task);
                    cancel_on_drop.disarm();
                    return Err(format!(
                        "{label} exceeded its full deadline and cleanup timeout"
                    ));
                }
            }
        }
    };
    cancel_on_drop.disarm();
    joined
        .map_err(|error| format!("{label} task failed: {error}"))?
        .map_err(|error| {
            if cancellation.is_cancelled() && error == MESH_OPERATION_CANCELLED_ERROR {
                format!("{label} exceeded its full deadline")
            } else {
                error
            }
        })
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
    let pool = pool.clone();
    let scope = scope.clone();
    run_owned_mesh_operation("pairwise SSH mesh check", move |cancellation| async move {
        pairwise_ssh_check_scoped_owned(&pool, &scope, &cancellation).await
    })
    .await
}

async fn pairwise_ssh_check_scoped_owned(
    pool: &PgPool,
    scope: &MeshCheckScope,
    cancellation: &CancellationToken,
) -> Result<MeshMatrix, String> {
    let fence = acquire_mesh_scan_fence(pool, cancellation).await?;
    let result = async {
        let nodes =
            bounded_mesh_step(cancellation, "list mesh nodes", ff_db::pg_list_nodes(pool)).await?;
        bounded_mesh_step(
            cancellation,
            "mark ineligible mesh pairs skipped",
            mark_ineligible_pairs_skipped(pool, &nodes, scope.exclusions()),
        )
        .await?;
        let matrix = pairwise_ssh_check_inner(pool, &nodes, scope, cancellation.clone()).await?;
        bounded_mesh_step(
            cancellation,
            "evaluate mesh alert",
            fire_mesh_alert_scoped(pool, scope.exclusions()),
        )
        .await?;
        Ok(matrix)
    }
    .await;
    finish_mesh_scan(fence, result).await
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
/// run concurrently up to the global cap.
///
/// A transient SSH pre-auth failure opens a short endpoint-scoped circuit
/// breaker. The failed edge is retried once after the cooldown; if that
/// half-open attempt also fails, other incident edges remain deferred for one
/// more cooldown instead of immediately hammering the same sshd. The scan-wide
/// deadline makes both retries and cooldowns operationally bounded.
async fn run_bounded_mesh_probes<F, Fut>(
    probes: Vec<MeshProbe>,
    policy: MeshSchedulePolicy,
    cancellation: CancellationToken,
    run_probe: F,
) -> MeshScheduleResult
where
    F: Fn(MeshProbe, CancellationToken) -> Fut,
    Fut: Future<Output = MeshCell>,
{
    use futures::stream::{FuturesUnordered, StreamExt};

    let mut pending: VecDeque<ScheduledMeshProbe> = probes
        .into_iter()
        .map(|probe| ScheduledMeshProbe {
            probe,
            transient_retries: 0,
        })
        .collect();
    let mut active_nodes = BTreeSet::new();
    let mut active_probes = BTreeMap::new();
    let mut endpoint_cooldowns = BTreeMap::<String, Instant>::new();
    let mut in_flight = FuturesUnordered::new();
    let mut cells = Vec::with_capacity(pending.len());
    let mut persistable_edges = BTreeSet::new();
    let max_in_flight = policy.max_in_flight.max(1);
    let deadline = Instant::now() + policy.scan_deadline;
    let scan_cancel = cancellation.child_token();
    let mut deadline_expired = false;

    while !pending.is_empty() || !in_flight.is_empty() {
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            deadline_expired = true;
            break;
        }

        while in_flight.len() < max_in_flight {
            let now = Instant::now();
            let Some(index) = pending.iter().position(|scheduled| {
                let probe = &scheduled.probe;
                !active_nodes.contains(&probe.src)
                    && !active_nodes.contains(&probe.dst)
                    && endpoint_ready(&endpoint_cooldowns, &probe.src, now)
                    && endpoint_ready(&endpoint_cooldowns, &probe.dst, now)
            }) else {
                break;
            };
            let scheduled = pending
                .remove(index)
                .expect("selected mesh probe index remains valid");
            let probe = &scheduled.probe;
            let src = probe.src.clone();
            let dst = probe.dst.clone();
            active_nodes.insert(src.clone());
            active_nodes.insert(dst.clone());
            active_probes.insert((src.clone(), dst.clone()), probe.clone());
            let future = run_probe(probe.clone(), scan_cancel.child_token());
            in_flight.push(async move { (src, dst, scheduled, future.await) });
        }

        if in_flight.is_empty() {
            let Some(wake_at) = pending
                .iter()
                .map(|scheduled| {
                    probe_ready_at(&endpoint_cooldowns, &scheduled.probe, Instant::now())
                })
                .min()
            else {
                break;
            };
            tokio::select! {
                _ = cancellation.cancelled() => {
                    deadline_expired = true;
                    break;
                }
                _ = tokio::time::sleep_until(wake_at.min(deadline)) => {}
            }
            continue;
        }

        let next = tokio::select! {
            _ = cancellation.cancelled() => {
                deadline_expired = true;
                None
            }
            result = timeout(
                deadline.saturating_duration_since(Instant::now()),
                in_flight.next(),
            ) => match result {
                Ok(next) => next,
                Err(_) => {
                    deadline_expired = true;
                    None
                }
            }
        };
        if deadline_expired && next.is_none() {
            break;
        }
        let Some((src, dst, mut scheduled, cell)) = next else {
            debug_assert!(pending.is_empty());
            break;
        };
        active_nodes.remove(&src);
        active_nodes.remove(&dst);
        active_probes.remove(&(src, dst));

        if let Some(scope) = transient_mesh_failure_scope(&cell) {
            let cooldown_until = Instant::now() + policy.transient_cooldown;
            apply_endpoint_cooldown(
                &mut endpoint_cooldowns,
                &scheduled.probe,
                scope,
                cooldown_until,
            );
            if scheduled.transient_retries < policy.max_transient_retries {
                scheduled.transient_retries += 1;
                // The same edge is the single half-open probe when its
                // endpoint cooldown expires; keep it ahead of other incident
                // edges that were already pending.
                pending.push_front(scheduled);
                continue;
            }
        }
        persistable_edges.insert((cell.src.clone(), cell.dst.clone()));
        cells.push(cell);
    }

    if deadline_expired {
        // Cancellation is cooperative and owned: every active probe kills and
        // reaps its child; pipe reads live inside that probe future rather than
        // detached tasks. Drain all futures so no SSH work survives return.
        scan_cancel.cancel();
        while let Some((src, dst, scheduled, cell)) = in_flight.next().await {
            active_nodes.remove(&src);
            active_nodes.remove(&dst);
            active_probes.remove(&(src, dst));
            let exhausted_transient_retry = transient_mesh_failure_scope(&cell).is_some()
                && scheduled.transient_retries >= policy.max_transient_retries;
            if cell.last_error.as_deref() != Some(MESH_SCAN_DEADLINE_ERROR)
                && (transient_mesh_failure_scope(&cell).is_none() || exhausted_transient_retry)
            {
                persistable_edges.insert((cell.src.clone(), cell.dst.clone()));
            }
            cells.push(cell);
        }

        let mut unfinished = BTreeMap::<(String, String), MeshProbe>::new();
        for scheduled in pending {
            unfinished.insert(
                (scheduled.probe.src.clone(), scheduled.probe.dst.clone()),
                scheduled.probe,
            );
        }
        debug_assert!(active_probes.is_empty());
        cells.extend(unfinished.into_values().map(mesh_deadline_cell));
    }

    cells.sort_by(|a, b| (&a.src, &a.dst).cmp(&(&b.src, &b.dst)));
    MeshScheduleResult {
        cells,
        persistable_edges,
    }
}

fn endpoint_ready(cooldowns: &BTreeMap<String, Instant>, endpoint: &str, now: Instant) -> bool {
    cooldowns
        .get(endpoint)
        .map(|ready_at| *ready_at <= now)
        .unwrap_or(true)
}

fn probe_ready_at(
    cooldowns: &BTreeMap<String, Instant>,
    probe: &MeshProbe,
    now: Instant,
) -> Instant {
    let src = cooldowns.get(&probe.src).copied().unwrap_or(now);
    let dst = cooldowns.get(&probe.dst).copied().unwrap_or(now);
    src.max(dst)
}

fn apply_endpoint_cooldown(
    cooldowns: &mut BTreeMap<String, Instant>,
    probe: &MeshProbe,
    scope: TransientFailureScope,
    ready_at: Instant,
) {
    let mut apply = |endpoint: &str| {
        cooldowns
            .entry(endpoint.to_string())
            .and_modify(|current| *current = (*current).max(ready_at))
            .or_insert(ready_at);
    };
    match scope {
        TransientFailureScope::Source => apply(&probe.src),
        TransientFailureScope::Destination => apply(&probe.dst),
        TransientFailureScope::Both => {
            apply(&probe.src);
            apply(&probe.dst);
        }
    }
}

fn transient_mesh_failure_scope(cell: &MeshCell) -> Option<TransientFailureScope> {
    if cell.status == "ok" {
        return None;
    }
    let error = cell.last_error.as_deref()?.to_ascii_lowercase();
    if error.trim() == "timeout" {
        // The scan-wide timeout can fire during either the outer or nested SSH
        // handshake, before the remote marker is collected.
        return Some(TransientFailureScope::Both);
    }
    let is_transient_preauth = [
        "banner exchange",
        "kex_exchange_identification",
        "ssh_exchange_identification",
        "connection timed out",
        "connection reset",
        "connection closed by",
    ]
    .iter()
    .any(|needle| error.contains(needle));
    if !is_transient_preauth {
        return None;
    }
    // A remote marker proves the outer hop reached src and ran the nested
    // probe, so its transient failure belongs to dst. Without it, the outer
    // source hop failed before the remote shell completed.
    Some(if cell.ping_ok.is_some() {
        TransientFailureScope::Destination
    } else {
        TransientFailureScope::Source
    })
}

fn mesh_deadline_cell(probe: MeshProbe) -> MeshCell {
    MeshCell {
        src: probe.src,
        dst: probe.dst,
        status: "failed".into(),
        last_error: Some(MESH_SCAN_DEADLINE_ERROR.into()),
        ping_ok: None,
        ssh_ok: false,
    }
}

async fn persist_mesh_cells(
    pool: &PgPool,
    cells: Vec<MeshCell>,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    // This is the sole mesh-status upsert call site in ff-agent. Every caller
    // holds MESH_SCAN_FENCE_KEY, covering full scans, local scans, propagation,
    // and deferred single-edge retries so a stale writer cannot race a scan.
    use futures::stream::{self, StreamExt};

    if cells.is_empty() {
        return Ok(());
    }
    let writes = stream::iter(cells).map(|cell| async move {
        timeout(
            MESH_PERSIST_WRITE_TIMEOUT,
            ff_db::pg_upsert_mesh_probe(
                pool,
                &cell.src,
                &cell.dst,
                &cell.status,
                cell.last_error.as_deref(),
                cell.ping_ok,
                Some(cell.ssh_ok),
            ),
        )
        .await
        .map_err(|_| format!("persist mesh edge {} -> {} timed out", cell.src, cell.dst))?
        .map_err(|error| format!("persist mesh edge {} -> {}: {error}", cell.src, cell.dst))
    });
    let results = tokio::select! {
        _ = cancellation.cancelled() => return Err(MESH_OPERATION_CANCELLED_ERROR.into()),
        result = timeout(
            MESH_PERSIST_GLOBAL_TIMEOUT,
            writes
                .buffer_unordered(MESH_PERSIST_MAX_IN_FLIGHT)
                .collect::<Vec<_>>(),
        ) => result.map_err(|_| "mesh persistence exceeded its global deadline".to_string())?,
    };
    let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn pairwise_ssh_check_inner(
    pool: &PgPool,
    nodes: &[ff_db::FleetNodeRow],
    scope: &MeshCheckScope,
    cancellation: CancellationToken,
) -> Result<MeshMatrix, String> {
    let probes = mesh_probe_plan(nodes, scope);
    let schedule = run_bounded_mesh_probes(
        probes,
        PAIRWISE_MESH_SCHEDULE_POLICY,
        cancellation.clone(),
        |probe, cancellation| async move {
            probe_pair(
                probe.src,
                probe.src_user,
                probe.src_ip,
                probe.dst,
                probe.dst_user,
                probe.dst_ip,
                cancellation,
            )
            .await
        },
    )
    .await;
    let persistable: Vec<MeshCell> = schedule
        .cells
        .iter()
        .filter(|cell| {
            schedule
                .persistable_edges
                .contains(&(cell.src.clone(), cell.dst.clone()))
        })
        .cloned()
        .collect();
    persist_mesh_cells(pool, persistable, &cancellation).await?;

    Ok(MeshMatrix {
        cells: schedule.cells,
        checked_at: chrono::Utc::now(),
    })
}

async fn read_child_pipe<R>(mut pipe: R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

#[derive(Debug)]
enum CommandOutcome {
    Completed(Output),
    TimedOut,
    Cancelled,
}

#[cfg(unix)]
fn process_group_exists(pid: u32) -> bool {
    // SAFETY: signal 0 does not mutate the process group; it only performs the
    // kernel's existence/permission check. EPERM still means that a member is
    // alive, while ESRCH proves that the group has disappeared.
    let result = unsafe { libc::kill(-(pid as i32), 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
async fn wait_for_process_group_exit(pid: u32) {
    while process_group_exists(pid) {
        // Do not turn a slow init/subreaper into false cleanup success. The
        // process-owning task keeps waiting after the outer 280-second
        // operation budget returns an error to the daemon.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(not(unix))]
async fn wait_for_process_group_exit(_pid: u32) {}

async fn terminate_and_reap_child(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
) -> std::io::Result<()> {
    if let Some(pid) = pid {
        crate::task_runner::kill_process_group(pid);
    }
    if let Err(error) = child.start_kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        warn!(%error, "mesh command child could not be killed");
    }

    // Once SIGKILL has been sent, waiting for this direct child is not an
    // optional/best-effort cleanup step. Returning before wait(2) observes it
    // would leak a zombie. The enclosing operation retains its independent
    // 300-second watchdog contract; under Unix a SIGKILLed child cannot keep
    // executing or ignore termination.
    child.wait().await?;
    if let Some(pid) = pid {
        wait_for_process_group_exit(pid).await;
    }
    Ok(())
}

/// Run one SSH process with cancellation that owns the process lifecycle.
///
/// Tokio's timeout only cancels the Rust future. Without `kill_on_drop`, the
/// SSH child survives and its remote shell/nested SSH session can occupy an
/// sshd pre-auth slot until LoginGraceTime. On timeout, kill and reap the child
/// before releasing the scheduler's endpoint reservations.
async fn output_with_hard_timeout(
    mut command: Command,
    limit: Duration,
    cancellation: CancellationToken,
) -> std::io::Result<CommandOutcome> {
    if cancellation.is_cancelled() {
        return Ok(CommandOutcome::Cancelled);
    }
    #[cfg(unix)]
    command.process_group(0);
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_and_reap_child(&mut child, pid).await?;
            return Err(std::io::Error::other("SSH stdout pipe unavailable"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_and_reap_child(&mut child, pid).await?;
            return Err(std::io::Error::other("SSH stderr pipe unavailable"));
        }
    };

    enum WaitOutcome {
        Completed(std::io::Result<Output>),
        TimedOut,
        Cancelled,
    }
    // Keep pipe reads in this future rather than spawned tasks. Dropping the
    // future closes both pipes synchronously, so caller cancellation cannot
    // detach readers. `join!` polls status/stdout/stderr together and does not
    // skip stderr merely because stdout returned an error.
    let wait_outcome = {
        let collect_output = async {
            let (status, stdout, stderr) = tokio::join!(
                child.wait(),
                read_child_pipe(stdout),
                read_child_pipe(stderr)
            );
            Ok(Output {
                status: status?,
                stdout: stdout?,
                stderr: stderr?,
            })
        };
        tokio::pin!(collect_output);
        tokio::select! {
            biased;
            result = &mut collect_output => WaitOutcome::Completed(result),
            _ = cancellation.cancelled() => WaitOutcome::Cancelled,
            _ = tokio::time::sleep(limit) => WaitOutcome::TimedOut,
        }
    };

    match wait_outcome {
        WaitOutcome::Completed(Ok(output)) => Ok(CommandOutcome::Completed(output)),
        WaitOutcome::Completed(Err(error)) => {
            terminate_and_reap_child(&mut child, pid).await?;
            Err(error)
        }
        WaitOutcome::TimedOut => {
            terminate_and_reap_child(&mut child, pid).await?;
            Ok(CommandOutcome::TimedOut)
        }
        WaitOutcome::Cancelled => {
            terminate_and_reap_child(&mut child, pid).await?;
            Ok(CommandOutcome::Cancelled)
        }
    }
}

async fn probe_pair(
    src: String,
    src_user: String,
    src_ip: String,
    dst: String,
    dst_user: String,
    dst_ip: String,
    cancellation: CancellationToken,
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
    let mut command = Command::new("ssh");
    command.args(crate::ssh_opts::ssh_bypass_args()).args([
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
        &format!("{src_user}@{src_ip}"),
        &inner,
    ]);
    let probe_cancellation = cancellation.clone();
    let result = output_with_hard_timeout(command, MESH_SSH_PROBE_TIMEOUT, cancellation).await;
    if probe_cancellation.is_cancelled() {
        return MeshCell {
            ping_ok: None,
            ssh_ok: false,
            src,
            dst,
            status: "failed".into(),
            last_error: Some(MESH_SCAN_DEADLINE_ERROR.into()),
        };
    }

    match result {
        Ok(CommandOutcome::Completed(out)) if out.status.success() => MeshCell {
            ping_ok: parse_remote_probe_marker(&out.stdout).map(|(ping, _)| ping),
            ssh_ok: true,
            src,
            dst,
            status: "ok".into(),
            last_error: None,
        },
        Ok(CommandOutcome::Completed(out)) => MeshCell {
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
        Err(e) => MeshCell {
            ping_ok: None,
            ssh_ok: false,
            src,
            dst,
            status: "failed".into(),
            last_error: Some(format!("spawn: {e}")),
        },
        Ok(CommandOutcome::TimedOut) => MeshCell {
            ping_ok: None,
            ssh_ok: false,
            src,
            dst,
            status: "failed".into(),
            last_error: Some("timeout".into()),
        },
        Ok(CommandOutcome::Cancelled) => MeshCell {
            ping_ok: None,
            ssh_ok: false,
            src,
            dst,
            status: "failed".into(),
            last_error: Some(MESH_SCAN_DEADLINE_ERROR.into()),
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
    let pool = pool.clone();
    let scope = scope.clone();
    run_owned_mesh_operation(
        "local SSH reachability check",
        move |cancellation| async move {
            local_reach_check_scoped_owned(&pool, &scope, &cancellation).await
        },
    )
    .await
}

async fn local_reach_check_scoped_owned(
    pool: &PgPool,
    scope: &MeshCheckScope,
    cancellation: &CancellationToken,
) -> Result<Vec<LocalProbe>, String> {
    let fence = acquire_mesh_scan_fence(pool, cancellation).await?;
    let result = local_reach_check_scoped_inner(pool, scope, cancellation).await;
    finish_mesh_scan(fence, result).await
}

async fn local_reach_check_scoped_inner(
    pool: &PgPool,
    scope: &MeshCheckScope,
    cancellation: &CancellationToken,
) -> Result<Vec<LocalProbe>, String> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let me = crate::fleet_info::resolve_this_worker_name().await;
    let nodes = bounded_mesh_step(
        cancellation,
        "list local mesh nodes",
        ff_db::pg_list_nodes(pool),
    )
    .await?;
    bounded_mesh_step(
        cancellation,
        "mark local ineligible mesh pairs skipped",
        mark_ineligible_pairs_skipped(pool, &nodes, scope.exclusions()),
    )
    .await?;

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
            cancellation.child_token(),
        ));
        if futs.len() >= 8
            && let Some(p) = futs.next().await
            && let Some(p) = p
        {
            probes.push(p);
        }
    }
    while let Some(p) = futs.next().await {
        if let Some(p) = p {
            probes.push(p);
        }
    }
    let cells = probes
        .iter()
        .map(|probe| MeshCell {
            src: probe.src.clone(),
            dst: probe.dst.clone(),
            status: probe.status.clone(),
            last_error: probe.detail.clone(),
            ping_ok: Some(probe.ping_ok),
            ssh_ok: probe.ssh_ok,
        })
        .collect();
    persist_mesh_cells(pool, cells, cancellation).await?;
    bounded_mesh_step(
        cancellation,
        "evaluate local mesh alert",
        fire_mesh_alert_scoped(pool, scope.exclusions()),
    )
    .await?;
    probes.sort_by(|a, b| a.dst.cmp(&b.dst));
    Ok(probes)
}

async fn probe_direct(
    src: String,
    dst: String,
    dst_user: String,
    dst_ip: String,
    cancellation: CancellationToken,
) -> Option<LocalProbe> {
    // macOS ping -W is milliseconds; Linux is seconds.
    let ping_wait: &str = if cfg!(target_os = "macos") {
        "2000"
    } else {
        "2"
    };
    let mut ping = Command::new("ping");
    ping.args(["-c", "1", "-W", ping_wait, &dst_ip]);
    let ping_ok = matches!(
        output_with_hard_timeout(ping, Duration::from_secs(4), cancellation.child_token()).await,
        Ok(CommandOutcome::Completed(out)) if out.status.success()
    );
    if cancellation.is_cancelled() {
        return None;
    }

    let mut ssh = Command::new("ssh");
    ssh.args(crate::ssh_opts::ssh_bypass_args()).args([
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
        &format!("{dst_user}@{dst_ip}"),
        "true",
    ]);
    let ssh_res =
        output_with_hard_timeout(ssh, Duration::from_secs(8), cancellation.child_token()).await;
    if cancellation.is_cancelled() {
        return None;
    }
    let ssh_err = match ssh_res {
        Ok(CommandOutcome::Completed(out)) if out.status.success() => None,
        Ok(CommandOutcome::Completed(out)) => Some(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(120)
                .collect::<String>()
        )),
        Err(e) => Some(format!("spawn: {e}")),
        Ok(CommandOutcome::TimedOut) => Some("timeout".into()),
        // Cancellation is teardown, not a reachability observation. Returning
        // None prevents a synthetic failed cell from ever reaching the
        // persistence layer, even if cancellation races the caller's drain.
        Ok(CommandOutcome::Cancelled) => return None,
    };
    let ssh_ok = ssh_err.is_none();
    let (status, detail) = classify_direct_probe(ping_ok, ssh_err);
    Some(LocalProbe {
        src,
        dst,
        ip: dst_ip,
        ping_ok,
        ssh_ok,
        status,
        detail,
    })
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
    let pool = pool.clone();
    let params = params.clone();
    run_owned_mesh_operation("SSH mesh propagation", move |cancellation| async move {
        mesh_propagate_owned(&pool, &params, &cancellation).await
    })
    .await
}

async fn mesh_propagate_owned(
    pool: &PgPool,
    params: &serde_json::Value,
    cancellation: &CancellationToken,
) -> Result<(usize, usize), String> {
    let fence = acquire_mesh_scan_fence(pool, cancellation).await?;
    let result = mesh_propagate_inner(pool, params, cancellation).await;
    finish_mesh_scan(fence, result).await
}

async fn mesh_propagate_inner(
    pool: &PgPool,
    params: &serde_json::Value,
    cancellation: &CancellationToken,
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

    let nodes = bounded_mesh_step(
        cancellation,
        "list propagation mesh nodes",
        ff_db::pg_list_nodes(pool),
    )
    .await?;
    bounded_mesh_step(
        cancellation,
        "mark propagation ineligible mesh pairs skipped",
        mark_ineligible_pairs_skipped(pool, &nodes, &MeshExclusions::default()),
    )
    .await?;
    if nodes
        .iter()
        .find(|node| node.name == new_node)
        .is_some_and(|node| !mesh_eligible(node))
    {
        return Ok((0, 0));
    }
    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut cells = Vec::new();
    for peer in &nodes {
        if peer.name == new_node || !mesh_eligible(peer) {
            continue;
        }
        match propagate_to_peer(peer, user_key, &known_lines, new_user, new_ip, cancellation).await
        {
            Ok(observation) => {
                let (peer_ok, observed_cells) = propagation_cells(
                    &peer.name,
                    new_node,
                    observation.peer_to_new,
                    observation.new_to_peer,
                );
                if peer_ok {
                    ok += 1;
                } else {
                    fail += 1;
                }
                cells.extend(observed_cells);
            }
            Err(e) => {
                if cancellation.is_cancelled() {
                    return Err(MESH_OPERATION_CANCELLED_ERROR.into());
                }
                fail += 1;
                warn!(peer = %peer.name, %e, "mesh propagation setup failed before directional probes");
            }
        }
    }
    persist_mesh_cells(pool, cells, cancellation).await?;
    Ok((ok, fail))
}

#[derive(Debug)]
struct PropagationObservation {
    peer_to_new: Result<(), String>,
    new_to_peer: Result<(), String>,
}

fn observed_propagation_cell(src: &str, dst: &str, result: Result<(), String>) -> MeshCell {
    match result {
        Ok(()) => MeshCell {
            src: src.to_string(),
            dst: dst.to_string(),
            status: "ok".into(),
            last_error: None,
            ping_ok: None,
            ssh_ok: true,
        },
        Err(error) => MeshCell {
            src: src.to_string(),
            dst: dst.to_string(),
            status: "failed".into(),
            last_error: Some(error),
            ping_ok: None,
            ssh_ok: false,
        },
    }
}

fn propagation_cells(
    peer: &str,
    new_node: &str,
    peer_to_new: Result<(), String>,
    new_to_peer: Result<(), String>,
) -> (bool, [MeshCell; 2]) {
    let peer_to_new = observed_propagation_cell(peer, new_node, peer_to_new);
    let new_to_peer = observed_propagation_cell(new_node, peer, new_to_peer);
    (
        peer_to_new.ssh_ok && new_to_peer.ssh_ok,
        [peer_to_new, new_to_peer],
    )
}

async fn propagate_to_peer(
    peer: &ff_db::FleetNodeRow,
    user_key: &str,
    known_lines: &[String],
    new_user: &str,
    new_ip: &str,
    cancellation: &CancellationToken,
) -> Result<PropagationObservation, String> {
    let peer_dest = format!("{}@{}", peer.ssh_user, peer.ip);
    if !user_key.trim().is_empty() {
        let cmd = format!(
            "mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
             grep -Fq {quoted} ~/.ssh/authorized_keys || \
             echo {quoted} >> ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys",
            quoted = shell_escape_single(user_key),
        );
        ssh_exec(&peer_dest, &cmd, cancellation.child_token()).await?;
    }
    for line in known_lines {
        let cmd = format!(
            "touch ~/.ssh/known_hosts && \
             grep -Fq {quoted} ~/.ssh/known_hosts || \
             echo {quoted} >> ~/.ssh/known_hosts && \
             chmod 644 ~/.ssh/known_hosts",
            quoted = shell_escape_single(line),
        );
        ssh_exec(&peer_dest, &cmd, cancellation.child_token()).await?;
    }
    let peer_to_new_probe = format!(
        "ssh {} -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {new_user}@{new_ip} true",
        crate::ssh_opts::SSH_AGENT_BYPASS,
    );
    let peer_to_new = ssh_exec(&peer_dest, &peer_to_new_probe, cancellation.child_token()).await;
    if cancellation.is_cancelled() {
        return Err(MESH_OPERATION_CANCELLED_ERROR.into());
    }

    // The first nested SSH proves only peer -> new. Independently enter the
    // new node and SSH back to the peer before publishing the reverse edge;
    // directionality cannot be inferred from a successful opposite hop.
    let new_dest = format!("{new_user}@{new_ip}");
    let new_to_peer_probe = format!(
        "ssh {} -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {}@{} true",
        crate::ssh_opts::SSH_AGENT_BYPASS,
        peer.ssh_user,
        peer.ip,
    );
    let new_to_peer = ssh_exec(&new_dest, &new_to_peer_probe, cancellation.child_token()).await;
    if cancellation.is_cancelled() {
        return Err(MESH_OPERATION_CANCELLED_ERROR.into());
    }

    Ok(PropagationObservation {
        peer_to_new,
        new_to_peer,
    })
}

async fn ssh_exec(dest: &str, cmd: &str, cancellation: CancellationToken) -> Result<(), String> {
    let mut command = Command::new("ssh");
    command.args(crate::ssh_opts::ssh_bypass_args()).args([
        "-o",
        "ConnectTimeout=8",
        "-o",
        "StrictHostKeyChecking=accept-new",
        dest,
        cmd,
    ]);
    let out = match output_with_hard_timeout(command, Duration::from_secs(15), cancellation)
        .await
        .map_err(|e| format!("ssh spawn: {e}"))?
    {
        CommandOutcome::Completed(out) => out,
        CommandOutcome::TimedOut => return Err(format!("ssh to {dest} timed out")),
        CommandOutcome::Cancelled => return Err(format!("ssh to {dest} cancelled")),
    };
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
    let pool = pool.clone();
    let src = src.to_string();
    let dst = dst.to_string();
    run_owned_mesh_operation(
        "single-pair SSH mesh retry",
        move |cancellation| async move {
            probe_single_pair_owned(&pool, &src, &dst, &cancellation).await
        },
    )
    .await
}

async fn probe_single_pair_owned(
    pool: &PgPool,
    src: &str,
    dst: &str,
    cancellation: &CancellationToken,
) -> Result<MeshCell, String> {
    let fence = acquire_mesh_scan_fence(pool, cancellation).await?;
    let result = probe_single_pair_inner(pool, src, dst, cancellation).await;
    finish_mesh_scan(fence, result).await
}

async fn probe_single_pair_inner(
    pool: &PgPool,
    src: &str,
    dst: &str,
    cancellation: &CancellationToken,
) -> Result<MeshCell, String> {
    let nodes = bounded_mesh_step(
        cancellation,
        "list single-pair mesh nodes",
        ff_db::pg_list_nodes(pool),
    )
    .await?;
    bounded_mesh_step(
        cancellation,
        "mark single-pair ineligible mesh pairs skipped",
        mark_ineligible_pairs_skipped(pool, &nodes, &MeshExclusions::default()),
    )
    .await?;
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
        cancellation.child_token(),
    )
    .await;
    if cell.last_error.as_deref() == Some(MESH_SCAN_DEADLINE_ERROR) {
        return Err(MESH_OPERATION_CANCELLED_ERROR.into());
    }
    persist_mesh_cells(pool, vec![cell.clone()], cancellation).await?;
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
    let cancellation = CancellationToken::new();
    let nodes = bounded_mesh_step(
        &cancellation,
        "list retry mesh nodes",
        ff_db::pg_list_nodes(pool),
    )
    .await?;
    let fence = acquire_mesh_scan_fence(pool, &cancellation).await?;
    let mark_result = bounded_mesh_step(
        &cancellation,
        "mark retry ineligible mesh pairs skipped",
        mark_ineligible_pairs_skipped(pool, &nodes, scope.exclusions()),
    )
    .await;
    finish_mesh_scan(fence, mark_result).await?;
    enqueue_retries_scoped_inner(pool, scope, &nodes).await
}

async fn enqueue_retries_scoped_inner(
    pool: &PgPool,
    scope: &MeshCheckScope,
    nodes: &[ff_db::FleetNodeRow],
) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
    let retry_window = chrono::Utc::now() - chrono::Duration::hours(24);
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

        let policy = MeshSchedulePolicy {
            max_in_flight: PAIRWISE_MESH_MAX_IN_FLIGHT,
            transient_cooldown: Duration::ZERO,
            scan_deadline: Duration::from_secs(1),
            max_transient_retries: 0,
        };
        let run = run_bounded_mesh_probes(
            probes,
            policy,
            CancellationToken::new(),
            |probe, _cancellation| {
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
            },
        );
        let schedule = timeout(Duration::from_secs(2), run)
            .await
            .expect("bounded scheduler deadlocked");
        assert_eq!(
            schedule.persistable_edges.len(),
            names.len() * (names.len() - 1)
        );
        let cells = schedule.cells;

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

    fn scheduled_test_probe(src: &str, dst: &str) -> MeshProbe {
        MeshProbe {
            src: src.into(),
            src_user: "user".into(),
            src_ip: "127.0.0.1".into(),
            dst: dst.into(),
            dst_user: "user".into(),
            dst_ip: "127.0.0.1".into(),
        }
    }

    fn scheduled_test_cell(
        probe: MeshProbe,
        status: &str,
        last_error: Option<&str>,
        ping_ok: Option<bool>,
    ) -> MeshCell {
        MeshCell {
            src: probe.src,
            dst: probe.dst,
            status: status.into(),
            last_error: last_error.map(str::to_string),
            ping_ok,
            ssh_ok: status == "ok",
        }
    }

    #[test]
    fn transient_failures_are_scoped_to_the_observed_ssh_hop() {
        let destination_banner = scheduled_test_cell(
            scheduled_test_probe("a", "b"),
            "failed",
            Some("exit 255: Connection timed out during banner exchange"),
            Some(true),
        );
        assert_eq!(
            transient_mesh_failure_scope(&destination_banner),
            Some(TransientFailureScope::Destination)
        );

        let source_banner = scheduled_test_cell(
            scheduled_test_probe("a", "b"),
            "failed",
            Some("exit 255: kex_exchange_identification: Connection closed by remote host"),
            None,
        );
        assert_eq!(
            transient_mesh_failure_scope(&source_banner),
            Some(TransientFailureScope::Source)
        );

        let ambiguous_timeout = scheduled_test_cell(
            scheduled_test_probe("a", "b"),
            "failed",
            Some("timeout"),
            None,
        );
        assert_eq!(
            transient_mesh_failure_scope(&ambiguous_timeout),
            Some(TransientFailureScope::Both)
        );

        let auth_failure = scheduled_test_cell(
            scheduled_test_probe("a", "b"),
            "failed",
            Some("exit 255: Permission denied (publickey)"),
            None,
        );
        assert_eq!(transient_mesh_failure_scope(&auth_failure), None);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_endpoint_recovers_through_one_half_open_retry() {
        use std::sync::{Arc, Mutex};

        let started_at = Instant::now();
        let starts = Arc::new(Mutex::new(
            BTreeMap::<(String, String), Vec<Duration>>::new(),
        ));
        let probes = vec![
            scheduled_test_probe("a", "b"),
            scheduled_test_probe("a", "c"),
            scheduled_test_probe("d", "e"),
        ];
        let policy = MeshSchedulePolicy {
            max_in_flight: 8,
            transient_cooldown: Duration::from_secs(30),
            scan_deadline: Duration::from_secs(5 * 60),
            max_transient_retries: 1,
        };

        let schedule = run_bounded_mesh_probes(probes, policy, CancellationToken::new(), {
            let starts = Arc::clone(&starts);
            move |probe, _cancellation| {
                let starts = Arc::clone(&starts);
                async move {
                    let is_first_ab_attempt = {
                        let mut starts = starts.lock().expect("start record lock");
                        let edge_starts = starts
                            .entry((probe.src.clone(), probe.dst.clone()))
                            .or_default();
                        edge_starts.push(Instant::now().duration_since(started_at));
                        probe.src == "a" && probe.dst == "b" && edge_starts.len() == 1
                    };
                    if is_first_ab_attempt {
                        scheduled_test_cell(probe, "failed", Some("timeout"), None)
                    } else {
                        scheduled_test_cell(probe, "ok", None, Some(true))
                    }
                }
            }
        })
        .await;
        assert_eq!(
            schedule.persistable_edges.len(),
            3,
            "only one final logical observation per edge is persistable"
        );
        let cells = schedule.cells;

        assert_eq!(cells.len(), 3);
        let starts = starts.lock().expect("start record lock");
        assert_eq!(
            starts.get(&("a".into(), "b".into())).unwrap(),
            &[Duration::ZERO, Duration::from_secs(30)]
        );
        assert_eq!(
            starts.get(&("a".into(), "c".into())).unwrap(),
            &[Duration::from_secs(30)],
            "a recovered endpoint may serve the next edge after its half-open success"
        );
        assert_eq!(
            starts.get(&("d".into(), "e".into())).unwrap(),
            &[Duration::ZERO],
            "node-disjoint work must continue while a and b cool down"
        );
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.src == "a" && cell.dst == "b")
                .map(|cell| cell.status.as_str()),
            Some("ok")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_failure_neither_retries_nor_opens_endpoint_breaker() {
        use std::sync::{Arc, Mutex};

        let started_at = Instant::now();
        let starts = Arc::new(Mutex::new(Vec::new()));
        let probes = vec![
            scheduled_test_probe("a", "b"),
            scheduled_test_probe("a", "c"),
        ];
        let policy = MeshSchedulePolicy {
            max_in_flight: 8,
            transient_cooldown: Duration::from_secs(30),
            scan_deadline: Duration::from_secs(60),
            max_transient_retries: 1,
        };
        let schedule = run_bounded_mesh_probes(probes, policy, CancellationToken::new(), {
            let starts = Arc::clone(&starts);
            move |probe, _cancellation| {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .expect("start record lock")
                        .push((probe.dst.clone(), Instant::now().duration_since(started_at)));
                    if probe.dst == "b" {
                        scheduled_test_cell(
                            probe,
                            "failed",
                            Some("exit 255: Permission denied (publickey)"),
                            None,
                        )
                    } else {
                        scheduled_test_cell(probe, "ok", None, Some(true))
                    }
                }
            }
        })
        .await;
        assert_eq!(schedule.persistable_edges.len(), 2);
        let cells = schedule.cells;

        assert_eq!(cells.len(), 2);
        assert_eq!(
            starts.lock().expect("start record lock").as_slice(),
            &[("b".into(), Duration::ZERO), ("c".into(), Duration::ZERO)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn scan_deadline_bounds_cooldown_and_reports_unfinished_edge() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let policy = MeshSchedulePolicy {
            max_in_flight: 1,
            transient_cooldown: Duration::from_secs(30),
            scan_deadline: Duration::from_secs(10),
            max_transient_retries: 1,
        };
        let schedule = run_bounded_mesh_probes(
            vec![scheduled_test_probe("a", "b")],
            policy,
            CancellationToken::new(),
            {
                let attempts = Arc::clone(&attempts);
                move |probe, _cancellation| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    async move { scheduled_test_cell(probe, "failed", Some("timeout"), None) }
                }
            },
        )
        .await;
        assert!(
            schedule.persistable_edges.is_empty(),
            "an unprobed deadline cell must never overwrite the last observation"
        );
        let cells = schedule.cells;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(cells.len(), 1);
        assert_eq!(
            cells[0].last_error.as_deref(),
            Some(MESH_SCAN_DEADLINE_ERROR)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn hard_timeout_kills_and_reaps_process_tree_before_returning() {
        let pid_file = std::env::temp_dir().join(format!("ff-mesh-timeout-{}.pid", Uuid::new_v4()));
        let script = format!(
            "sleep 30 & descendant=$!; printf '%s %s' \"$$\" \"$descendant\" > {}; wait \"$descendant\"",
            shell_escape_single(pid_file.to_string_lossy().as_ref())
        );
        let mut command = Command::new("sh");
        command.args(["-c", &script]);

        let result = output_with_hard_timeout(
            command,
            Duration::from_millis(250),
            CancellationToken::new(),
        )
        .await
        .expect("local child should spawn");
        assert!(
            matches!(result, CommandOutcome::TimedOut),
            "sleeping child must hit the hard timeout"
        );

        let pids = std::fs::read_to_string(&pid_file).expect("process tree wrote pids");
        for pid in pids.split_whitespace() {
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "timed-out process-tree member {pid} still exists after helper returned"
            );
        }
        let _ = std::fs::remove_file(pid_file);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn scan_deadline_cancels_and_reaps_live_child_before_returning() {
        let pid_file =
            std::env::temp_dir().join(format!("ff-mesh-scan-cancel-{}.pid", Uuid::new_v4()));
        let policy = MeshSchedulePolicy {
            max_in_flight: 1,
            transient_cooldown: Duration::ZERO,
            scan_deadline: Duration::from_millis(250),
            max_transient_retries: 0,
        };
        let schedule = run_bounded_mesh_probes(
            vec![scheduled_test_probe("a", "b")],
            policy,
            CancellationToken::new(),
            {
                let pid_file = pid_file.clone();
                move |probe, cancellation| {
                    let pid_file = pid_file.clone();
                    async move {
                        let script = format!(
                            "printf '%s' \"$$\" > {}; exec sleep 30",
                            shell_escape_single(pid_file.to_string_lossy().as_ref())
                        );
                        let mut command = Command::new("sh");
                        command.args(["-c", &script]);
                        match output_with_hard_timeout(
                            command,
                            Duration::from_secs(30),
                            cancellation,
                        )
                        .await
                        .expect("local child should spawn")
                        {
                            CommandOutcome::Cancelled => mesh_deadline_cell(probe),
                            CommandOutcome::TimedOut => {
                                scheduled_test_cell(probe, "failed", Some("timeout"), None)
                            }
                            CommandOutcome::Completed(_) => {
                                scheduled_test_cell(probe, "ok", None, Some(true))
                            }
                        }
                    }
                }
            },
        )
        .await;

        assert!(schedule.persistable_edges.is_empty());
        assert_eq!(
            schedule.cells[0].last_error.as_deref(),
            Some(MESH_SCAN_DEADLINE_ERROR)
        );
        let pid = std::fs::read_to_string(&pid_file)
            .expect("child wrote pid before scan cancellation")
            .trim()
            .to_string();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "scan-cancelled child {pid} still exists after scheduler returned"
        );
        let _ = std::fs::remove_file(pid_file);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_outer_owned_operation_cancels_and_reaps_live_child() {
        let pid_file =
            std::env::temp_dir().join(format!("ff-mesh-outer-drop-{}.pid", Uuid::new_v4()));
        let operation_pid_file = pid_file.clone();
        let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
        let outer = tokio::spawn(run_owned_mesh_operation(
            "outer-drop regression",
            move |cancellation| async move {
                let policy = MeshSchedulePolicy {
                    max_in_flight: 1,
                    transient_cooldown: Duration::ZERO,
                    scan_deadline: Duration::from_secs(30),
                    max_transient_retries: 0,
                };
                let schedule = run_bounded_mesh_probes(
                    vec![scheduled_test_probe("outer", "child")],
                    policy,
                    cancellation,
                    move |probe, probe_cancellation| {
                        let operation_pid_file = operation_pid_file.clone();
                        async move {
                            let script = format!(
                                "printf '%s' \"$$\" > {}; exec sleep 30",
                                shell_escape_single(operation_pid_file.to_string_lossy().as_ref())
                            );
                            let mut command = Command::new("sh");
                            command.args(["-c", &script]);
                            match output_with_hard_timeout(
                                command,
                                Duration::from_secs(30),
                                probe_cancellation,
                            )
                            .await
                            .expect("owned scheduler child should spawn")
                            {
                                CommandOutcome::Cancelled => mesh_deadline_cell(probe),
                                CommandOutcome::TimedOut => {
                                    scheduled_test_cell(probe, "failed", Some("timeout"), None)
                                }
                                CommandOutcome::Completed(_) => {
                                    scheduled_test_cell(probe, "ok", None, Some(true))
                                }
                            }
                        }
                    },
                )
                .await;
                let _ = cleanup_tx.send(());
                if schedule.cells[0].last_error.as_deref() == Some(MESH_SCAN_DEADLINE_ERROR) {
                    Ok(())
                } else {
                    Err(format!("expected cancelled schedule, got {schedule:?}"))
                }
            },
        ));

        let pid = timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && pid.trim().parse::<u32>().is_ok()
                {
                    break pid.trim().to_string();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned operation child wrote pid");

        outer.abort();
        let _ = outer.await;
        timeout(Duration::from_secs(10), cleanup_rx)
            .await
            .expect("detached cleanup completed after caller drop")
            .expect("detached cleanup sender survived caller drop");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "owned child {pid} still existed after cleanup acknowledged completion"
        );
        let _ = std::fs::remove_file(pid_file);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn inherited_pipe_holder_cannot_make_normal_output_join_unbounded() {
        let pid_file =
            std::env::temp_dir().join(format!("ff-mesh-pipe-holder-{}.pid", Uuid::new_v4()));
        let script = format!(
            "sleep 30 & printf '%s' \"$!\" > {}; exit 0",
            shell_escape_single(pid_file.to_string_lossy().as_ref())
        );
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        let outcome = output_with_hard_timeout(
            command,
            Duration::from_millis(250),
            CancellationToken::new(),
        )
        .await
        .expect("pipe-holder command should spawn");
        assert!(matches!(outcome, CommandOutcome::TimedOut));
        let pid = std::fs::read_to_string(&pid_file)
            .expect("background pipe holder wrote pid")
            .trim()
            .to_string();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "background pipe holder {pid} remained in the process table after helper returned"
        );
        let _ = std::fs::remove_file(pid_file);
    }

    #[tokio::test(start_paused = true)]
    async fn owned_operation_enforces_full_deadline_and_cleanup_budget() {
        let started = Instant::now();
        let error = run_owned_mesh_operation("synthetic mesh tick", |cancellation| async move {
            cancellation.cancelled().await;
            Err::<(), String>(MESH_OPERATION_CANCELLED_ERROR.into())
        })
        .await
        .unwrap_err();
        assert!(error.contains("full deadline"));
        assert!(
            Instant::now().duration_since(started)
                <= FULL_MESH_OPERATION_DEADLINE + MESH_OPERATION_CLEANUP_TIMEOUT
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_timeout_returns_error_but_does_not_abort_cleanup_owner() {
        let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
        let started = Instant::now();
        let error = run_owned_mesh_operation("slow cleanup", move |cancellation| async move {
            cancellation.cancelled().await;
            tokio::time::sleep(MESH_OPERATION_CLEANUP_TIMEOUT + Duration::from_secs(1)).await;
            let _ = cleanup_tx.send(());
            Ok::<(), String>(())
        })
        .await
        .unwrap_err();

        assert!(error.contains("cleanup timeout"));
        assert_eq!(
            Instant::now().duration_since(started),
            FULL_MESH_OPERATION_DEADLINE + MESH_OPERATION_CLEANUP_TIMEOUT
        );
        timeout(Duration::from_secs(2), cleanup_rx)
            .await
            .expect("detached cleanup owner continued after the bounded error")
            .expect("cleanup owner retained its resources");
    }

    #[test]
    fn production_mesh_deadlines_fit_below_daemon_watchdog() {
        assert!(
            FULL_MESH_OPERATION_DEADLINE + MESH_OPERATION_CLEANUP_TIMEOUT
                < crate::daemon::WATCHDOG_TIMEOUT
        );
        assert!(PAIRWISE_MESH_SCAN_DEADLINE < FULL_MESH_OPERATION_DEADLINE);
    }

    #[test]
    fn full_mesh_entrypoints_share_a_nonblocking_transaction_fence() {
        assert!(MESH_SCAN_FENCE_SQL.contains("pg_try_advisory_xact_lock"));
        assert!(!MESH_SCAN_FENCE_SQL.contains("pg_advisory_lock("));
        assert!(!MESH_SCAN_BUSY_ERROR.is_empty());
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

    #[tokio::test]
    async fn cancelled_direct_probe_is_not_a_persistable_observation() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let probe = probe_direct(
            "local".into(),
            "peer".into(),
            "user".into(),
            "192.0.2.1".into(),
            cancellation,
        )
        .await;
        assert!(probe.is_none());
    }

    #[test]
    fn propagation_records_each_observed_direction_independently() {
        let (both_ok, cells) =
            propagation_cells("peer", "new", Ok(()), Err("reverse probe refused".into()));
        assert!(!both_ok);
        assert_eq!(
            (cells[0].src.as_str(), cells[0].dst.as_str()),
            ("peer", "new")
        );
        assert_eq!(cells[0].status, "ok");
        assert_eq!(
            (cells[1].src.as_str(), cells[1].dst.as_str()),
            ("new", "peer")
        );
        assert_eq!(cells[1].status, "failed");
        assert_eq!(
            cells[1].last_error.as_deref(),
            Some("reverse probe refused")
        );
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
