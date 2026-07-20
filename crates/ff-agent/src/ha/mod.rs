//! High-availability orchestration for ForgeFleet.
//!
//! Currently contains the backup orchestrator (Postgres + Redis
//! snapshots, distributed across the fleet via the deferred-task
//! queue). Future additions: replica-lag monitor, promote/demote
//! coordinator, failover state machine.

pub mod agent;
pub mod backup;
pub mod error_tracker;
pub mod handoff;
pub mod log_monitor;
pub mod manager;
pub mod mirror_service;
pub mod node_info;
pub mod periodic;
pub mod pg_failover;
pub mod repair;
pub mod restore_drill;
pub mod self_heal;
pub mod slot_manager;

#[cfg(test)]
mod integration_tests;

/// Gracefully release this computer's active work-item leases before an agent
/// restart.
///
/// Unlike stale-lease recovery, draining is an orderly handoff and therefore
/// does not consume an attempt. The lease release, slot cleanup, worktree
/// cleanup, and requeue are committed atomically so another agent can resume
/// the work immediately.
pub async fn drain_work_item_leases(
    pool: &sqlx::PgPool,
    computer_id: uuid::Uuid,
) -> Result<u64, sqlx::Error> {
    let drained: i64 = sqlx::query_scalar(
        "WITH draining AS (
             SELECT id, work_item_id, sub_agent_id, lease_state, endpoint, attempt, computer_id
               FROM work_item_leases
              WHERE computer_id = $1
                AND released_at IS NULL
              FOR UPDATE
         ), drained AS (
             UPDATE work_item_leases l
                SET lease_state = 'released',
                    released_at = NOW(),
                    release_reason = 'agent restart drain'
               FROM draining d
              WHERE l.id = d.id
          RETURNING d.work_item_id,
                    d.sub_agent_id,
                    d.lease_state AS from_status,
                    d.endpoint,
                    d.attempt,
                    l.release_reason,
                    d.computer_id
         ), lease_events AS (
             INSERT INTO work_item_events
                 (work_item_id, from_status, to_status, computer, attempt, detail)
             SELECT d.work_item_id,
                    d.from_status,
                    'lease_released',
                    c.name,
                    d.attempt,
                    jsonb_build_object(
                        'event_type', 'lease_released',
                        'endpoint', d.endpoint,
                        'lane', CASE
                            WHEN NULLIF(d.endpoint, '') IS NULL THEN NULL
                            WHEN d.endpoint LIKE 'cloud:%'
                              OR d.endpoint ~ '^(codex|claude|kimi|gemini|grok)(:|$)'
                              THEN 'cloud'
                            ELSE 'local'
                        END,
                        'attempt', d.attempt,
                        'release_reason', d.release_reason
                    )
               FROM drained d
               LEFT JOIN computers c ON c.id = d.computer_id
         ), freed_slots AS (
             UPDATE sub_agents AS sa
                SET current_work_item_id = NULL,
                    status = 'idle',
                    started_at = NULL,
                    last_heartbeat_at = NOW()
              WHERE EXISTS (
                    SELECT 1
                      FROM drained AS d
                     WHERE d.sub_agent_id = sa.id
                       AND d.work_item_id = sa.current_work_item_id)
         ), retired_worktrees AS (
             UPDATE work_item_worktrees AS wt
                SET status = 'failed'
              WHERE wt.status IN ('creating', 'active')
                AND EXISTS (
                    SELECT 1 FROM drained AS d WHERE d.work_item_id = wt.work_item_id)
         ), requeued AS (
             UPDATE work_items AS wi
                SET status = 'ready',
                    assigned_computer = NULL
              WHERE wi.status IN ('claimed', 'building')
                AND EXISTS (
                    SELECT 1 FROM drained AS d WHERE d.work_item_id = wi.id)
         )
         SELECT COUNT(*) FROM drained",
    )
    .bind(computer_id)
    .fetch_one(pool)
    .await?;

    Ok(drained as u64)
}

// ─── Git mirror rewrite configuration ────────────────────────────────────────

/// Register Git `url.<mirror>.insteadOf` rewrite rules so that clones and
/// fetches against `github.com` are redirected to the LAN mirror.
///
/// Both common GitHub URL forms are rewritten:
///
/// * `https://github.com/<owner>/<repo>`
/// * `git@github.com:<owner>/<repo>`
///
/// The `mirror` argument must be the replacement URL prefix in the form Git
/// expects for `url.<base>.insteadOf`, e.g. `https://git-mirror.local/` or
/// `git@git-mirror.local:`.
pub async fn register_github_mirror_rewrite(mirror: &str) -> anyhow::Result<()> {
    if mirror.is_empty() {
        return Err(anyhow::anyhow!("mirror URL must not be empty"));
    }

    const GITHUB_PREFIXES: &[&str] = &["https://github.com/", "git@github.com:"];

    for original in GITHUB_PREFIXES {
        let key = format!("url.{mirror}.insteadOf");
        let status = tokio::process::Command::new("git")
            .args(["config", "--global", &key, original])
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn git config for {original}: {e}"))?;

        if !status.success() {
            return Err(anyhow::anyhow!(
                "git config --global url.{mirror}.insteadOf {original} failed ({status})"
            ));
        }
    }

    Ok(())
}

// ─── Pure HA topology model (used by tests + planners) ───────────────────────

/// Role of a database node in a fleet HA topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaRole {
    Primary,
    Replica,
    Standby,
}

/// In-memory representation of a single Postgres node for HA planning/tests.
#[derive(Debug, Clone)]
pub struct ReplicaNode {
    pub name: String,
    pub role: ReplicaRole,
    /// Replica lag in bytes. Meaningful only when role == ReplicaRole::Replica.
    pub lag_bytes: i64,
    /// True when the node is believed to be alive and its Postgres is reachable.
    pub healthy: bool,
}

impl ReplicaNode {
    /// Convenience constructor for tests and planners.
    pub fn new(name: &str, role: ReplicaRole, lag_bytes: i64, healthy: bool) -> Self {
        Self {
            name: name.to_string(),
            role,
            lag_bytes,
            healthy,
        }
    }
}

/// Outcome of evaluating a failover for a 3-replica style topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverDecision {
    /// A healthy, caught-up replica was selected for promotion.
    Promote { target: String },
    /// No replica is healthy enough to take over.
    NoHealthyReplica,
    /// No replica row exists at all.
    NoReplica,
    /// Replicas exist but all are too far behind the primary.
    AllReplicasLagging,
}

/// Pure decision logic: given a failed primary and a list of replicas, pick the
/// best replica to promote.
///
/// Selection criteria:
/// 1. Must be healthy.
/// 2. Must currently be a replica (not already primary/standby).
/// 3. Lag must be within `max_lag_bytes`.
/// 4. Among candidates, pick the one with the smallest lag.
pub fn choose_failover_target(
    replicas: &[ReplicaNode],
    max_lag_bytes: i64,
) -> Option<&ReplicaNode> {
    replicas
        .iter()
        .filter(|r| r.role == ReplicaRole::Replica && r.healthy && r.lag_bytes <= max_lag_bytes)
        .min_by_key(|r| r.lag_bytes)
}

/// Evaluate a failover for a 3-replica topology where `failed_primary` is the
/// name of the node that was previously primary.
///
/// This is a pure, testable summary of the decision the real
/// `pg_failover::PostgresFailoverManager` makes after ODOWN + TCP-unreachable
/// checks have already passed.
pub fn evaluate_failover(replicas: &[ReplicaNode], max_lag_bytes: i64) -> FailoverDecision {
    let replica_count = replicas
        .iter()
        .filter(|r| r.role == ReplicaRole::Replica)
        .count();
    if replica_count == 0 {
        return FailoverDecision::NoReplica;
    }

    let healthy_count = replicas
        .iter()
        .filter(|r| r.role == ReplicaRole::Replica && r.healthy)
        .count();
    if healthy_count == 0 {
        return FailoverDecision::NoHealthyReplica;
    }

    let caught_up_count = replicas
        .iter()
        .filter(|r| r.role == ReplicaRole::Replica && r.healthy && r.lag_bytes <= max_lag_bytes)
        .count();
    if caught_up_count == 0 {
        return FailoverDecision::AllReplicasLagging;
    }

    match choose_failover_target(replicas, max_lag_bytes) {
        Some(r) => FailoverDecision::Promote {
            target: r.name.clone(),
        },
        None => FailoverDecision::NoHealthyReplica,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_drain_is_attempt_neutral() {
        let source = include_str!("mod.rs");
        let drain = source
            .split("pub async fn drain_work_item_leases")
            .nth(1)
            .expect("lease drain function")
            .split("// ─── Pure HA topology model")
            .next()
            .expect("lease drain function body");

        assert!(!drain.contains("attempts = attempts + 1"));
        assert!(drain.contains("status = 'ready'"));
        assert!(drain.contains("released_at = NOW()"));
    }

    fn primary(name: &str) -> ReplicaNode {
        ReplicaNode::new(name, ReplicaRole::Primary, 0, true)
    }

    fn replica(name: &str, lag_bytes: i64) -> ReplicaNode {
        ReplicaNode::new(name, ReplicaRole::Replica, lag_bytes, true)
    }

    fn unhealthy_replica(name: &str, lag_bytes: i64) -> ReplicaNode {
        ReplicaNode::new(name, ReplicaRole::Replica, lag_bytes, false)
    }

    #[test]
    fn three_replicas_failover_to_caught_up_replica() {
        // Primary has failed. Two replicas are available: charlie is caught up,
        // delta is slightly behind. The failover planner should pick charlie.
        let nodes = vec![
            primary("alpha"),
            replica("bravo", 0),
            replica("charlie", 1_024),
        ];

        let decision = evaluate_failover(&nodes, 256 * 1_024);
        assert_eq!(
            decision,
            FailoverDecision::Promote {
                target: "bravo".to_string()
            }
        );
    }

    #[test]
    fn three_replicas_failover_picks_lowest_lag() {
        let nodes = vec![
            primary("alpha"),
            replica("bravo", 50_000),
            replica("charlie", 10_000),
            replica("delta", 40_000),
        ];

        let decision = evaluate_failover(&nodes, 256 * 1_024);
        assert_eq!(
            decision,
            FailoverDecision::Promote {
                target: "charlie".to_string()
            }
        );
    }

    #[test]
    fn three_replicas_no_failover_when_both_replicas_lag_too_high() {
        let nodes = vec![
            primary("alpha"),
            replica("bravo", 512 * 1_024),
            replica("charlie", 1_024 * 1_024),
        ];

        let decision = evaluate_failover(&nodes, 256 * 1_024);
        assert_eq!(decision, FailoverDecision::AllReplicasLagging);
    }

    #[test]
    fn three_replicas_no_failover_when_only_primary_remains() {
        let nodes = vec![primary("alpha")];

        let decision = evaluate_failover(&nodes, 256 * 1_024);
        assert_eq!(decision, FailoverDecision::NoReplica);
    }

    #[test]
    fn three_replicas_failover_skips_unhealthy_replica() {
        // bravo has the lowest lag but is unhealthy; charlie should be promoted.
        let nodes = vec![
            primary("alpha"),
            unhealthy_replica("bravo", 0),
            replica("charlie", 5_000),
        ];

        let decision = evaluate_failover(&nodes, 256 * 1_024);
        assert_eq!(
            decision,
            FailoverDecision::Promote {
                target: "charlie".to_string()
            }
        );
    }

    #[test]
    fn three_replicas_no_failover_when_all_replicas_unhealthy() {
        let nodes = vec![
            primary("alpha"),
            unhealthy_replica("bravo", 0),
            unhealthy_replica("charlie", 1_024),
        ];

        let decision = evaluate_failover(&nodes, 256 * 1_024);
        assert_eq!(decision, FailoverDecision::NoHealthyReplica);
    }

    #[test]
    fn choose_failover_target_returns_none_for_empty_slice() {
        assert!(choose_failover_target(&[], 256 * 1_024).is_none());
    }

    #[test]
    fn choose_failover_target_ignores_primary_and_standby() {
        let nodes = vec![
            primary("alpha"),
            ReplicaNode::new("bravo", ReplicaRole::Standby, 0, true),
            replica("charlie", 1_024),
        ];

        let target = choose_failover_target(&nodes, 256 * 1_024);
        assert_eq!(target.map(|r| r.name.as_str()), Some("charlie"));
    }
}
