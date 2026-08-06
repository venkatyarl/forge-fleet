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
pub mod jira_config;
pub mod jira_ingest;
pub mod log_monitor;
pub mod manager;
pub mod mirror_service;
pub mod node_drain;
pub mod node_info;
pub mod offline_mode;
pub mod periodic;
pub mod pg_failover;
pub mod pipeline_digest;
pub mod repair;
pub mod replica_monitor;
pub mod restore_drill;
pub mod self_heal;
pub mod slot_manager;
pub mod status_updater;

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod health;

#[cfg(test)]
mod self_heal_tests;

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
                    NULLIF(concat_ws('/', NULLIF(d.endpoint, ''), CASE
                        WHEN NULLIF(d.endpoint, '') IS NULL THEN NULL
                        WHEN d.endpoint LIKE 'cloud:%'
                          OR d.endpoint ~ '^(codex|claude|kimi|gemini|grok)(:|$)'
                          THEN 'cloud'
                        ELSE 'local'
                    END), '')
               FROM drained d
               LEFT JOIN computers c ON c.id = d.computer_id
         ), freed_slots AS (
             UPDATE sub_agents AS sa
                SET current_work_item_id = NULL,
                    status = CASE WHEN status = 'disabled' THEN 'disabled' ELSE 'idle' END,
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

/// Complete, independently-observed evidence required before automatic
/// promotion.  Every optional field is deliberate: missing telemetry must
/// reject a candidate rather than silently becoming a favourable default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverCandidateEvidence {
    pub stable_id: String,
    pub name: String,
    pub role: ReplicaRole,
    pub status: String,
    pub lag_bytes: Option<i64>,
    pub last_sync_age_secs: Option<i64>,
    pub in_recovery: Option<bool>,
    pub read_only: Option<bool>,
    pub streaming: Option<bool>,
    pub replay_lsn: Option<u64>,
    pub replay_age_secs: Option<i64>,
    pub receiver_age_secs: Option<i64>,
}

/// Bounds for evidence accepted by automatic failover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailoverEvidenceThresholds {
    pub max_lag_bytes: i64,
    pub max_last_sync_age_secs: i64,
    pub max_replay_age_secs: i64,
    pub max_receiver_age_secs: i64,
}

/// Why a replica is not safe to promote automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateRejection {
    WrongRole,
    NotRunning,
    Missing(&'static str),
    Invalid(&'static str),
    TooStale {
        evidence: &'static str,
        age_secs: i64,
        max_secs: i64,
    },
    TooFarBehind {
        lag_bytes: i64,
        max_bytes: i64,
    },
}

/// Validate every authority, recovery and freshness signal for a candidate.
pub fn reject_failover_candidate(
    candidate: &FailoverCandidateEvidence,
    thresholds: FailoverEvidenceThresholds,
) -> Option<CandidateRejection> {
    if candidate.role != ReplicaRole::Replica {
        return Some(CandidateRejection::WrongRole);
    }
    if candidate.status != "running" {
        return Some(CandidateRejection::NotRunning);
    }
    let lag_bytes = match candidate.lag_bytes {
        Some(value) if value >= 0 => value,
        Some(_) => return Some(CandidateRejection::Invalid("lag_bytes")),
        None => return Some(CandidateRejection::Missing("lag_bytes")),
    };
    if lag_bytes > thresholds.max_lag_bytes {
        return Some(CandidateRejection::TooFarBehind {
            lag_bytes,
            max_bytes: thresholds.max_lag_bytes,
        });
    }
    let last_sync_age_secs = match candidate.last_sync_age_secs {
        Some(value) if value >= 0 => value,
        Some(_) => return Some(CandidateRejection::Invalid("last_sync_at")),
        None => return Some(CandidateRejection::Missing("last_sync_at")),
    };
    if last_sync_age_secs > thresholds.max_last_sync_age_secs {
        return Some(CandidateRejection::TooStale {
            evidence: "last_sync_at",
            age_secs: last_sync_age_secs,
            max_secs: thresholds.max_last_sync_age_secs,
        });
    }
    match candidate.in_recovery {
        Some(true) => {}
        Some(false) => return Some(CandidateRejection::Invalid("pg_is_in_recovery")),
        None => return Some(CandidateRejection::Missing("pg_is_in_recovery")),
    }
    match candidate.read_only {
        Some(true) => {}
        Some(false) => return Some(CandidateRejection::Invalid("transaction_read_only")),
        None => return Some(CandidateRejection::Missing("transaction_read_only")),
    }
    match candidate.streaming {
        Some(true) => {}
        Some(false) => return Some(CandidateRejection::Invalid("walreceiver_streaming")),
        None => return Some(CandidateRejection::Missing("walreceiver_streaming")),
    }
    if candidate.replay_lsn.is_none() {
        return Some(CandidateRejection::Missing("last_replay_lsn"));
    }
    for (evidence, age, max_secs) in [
        (
            "last_replay",
            candidate.replay_age_secs,
            thresholds.max_replay_age_secs,
        ),
        (
            "walreceiver_message",
            candidate.receiver_age_secs,
            thresholds.max_receiver_age_secs,
        ),
    ] {
        let Some(age_secs) = age else {
            return Some(CandidateRejection::Missing(evidence));
        };
        if age_secs < 0 {
            return Some(CandidateRejection::Invalid(evidence));
        }
        if age_secs > max_secs {
            return Some(CandidateRejection::TooStale {
                evidence,
                age_secs,
                max_secs,
            });
        }
    }
    None
}

/// Pick the safest eligible replica deterministically.
///
/// Ordering is highest replay LSN, then lowest measured lag, replay age,
/// receiver age, name and stable computer id.  The last two keys guarantee
/// every leader independently reaches the same answer on identical evidence.
pub fn choose_evidenced_failover_target(
    candidates: &[FailoverCandidateEvidence],
    thresholds: FailoverEvidenceThresholds,
) -> Option<&FailoverCandidateEvidence> {
    candidates
        .iter()
        .filter(|candidate| reject_failover_candidate(candidate, thresholds).is_none())
        .min_by(|left, right| {
            right
                .replay_lsn
                .cmp(&left.replay_lsn)
                .then_with(|| left.lag_bytes.cmp(&right.lag_bytes))
                .then_with(|| left.replay_age_secs.cmp(&right.replay_age_secs))
                .then_with(|| left.receiver_age_secs.cmp(&right.receiver_age_secs))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.stable_id.cmp(&right.stable_id))
        })
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

    #[test]
    fn restart_drain_keeps_disabled_sub_agents_disabled() {
        // `fleet deploy --graceful` disables target sub-agents before draining
        // their leases; the slot cleanup must not flip them back to 'idle'.
        let source = include_str!("mod.rs");
        let drain = source
            .split("pub async fn drain_work_item_leases")
            .nth(1)
            .expect("lease drain function")
            .split("// ─── Pure HA topology model")
            .next()
            .expect("lease drain function body");

        assert!(!drain.contains("status = 'idle'"));
        assert!(
            drain
                .contains("status = CASE WHEN status = 'disabled' THEN 'disabled' ELSE 'idle' END")
        );
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

    fn complete_evidence(
        name: &str,
        stable_id: &str,
        replay_lsn: u64,
    ) -> FailoverCandidateEvidence {
        FailoverCandidateEvidence {
            stable_id: stable_id.into(),
            name: name.into(),
            role: ReplicaRole::Replica,
            status: "running".into(),
            lag_bytes: Some(16_384),
            last_sync_age_secs: Some(10),
            in_recovery: Some(true),
            read_only: Some(true),
            streaming: Some(true),
            replay_lsn: Some(replay_lsn),
            replay_age_secs: Some(4),
            receiver_age_secs: Some(1),
        }
    }

    fn evidence_thresholds() -> FailoverEvidenceThresholds {
        FailoverEvidenceThresholds {
            max_lag_bytes: 256 * 1_024,
            max_last_sync_age_secs: 300,
            max_replay_age_secs: 300,
            max_receiver_age_secs: 300,
        }
    }

    #[test]
    fn evidenced_selector_rejects_each_incomplete_or_unsafe_signal() {
        let base = complete_evidence("safe", "1", 100);
        let mutations: Vec<Box<dyn Fn(&mut FailoverCandidateEvidence)>> = vec![
            Box::new(|value| value.status = "degraded".into()),
            Box::new(|value| value.lag_bytes = None),
            Box::new(|value| value.lag_bytes = Some(256 * 1_024 + 1)),
            Box::new(|value| value.last_sync_age_secs = None),
            Box::new(|value| value.last_sync_age_secs = Some(301)),
            Box::new(|value| value.in_recovery = Some(false)),
            Box::new(|value| value.read_only = Some(false)),
            Box::new(|value| value.streaming = Some(false)),
            Box::new(|value| value.replay_lsn = None),
            Box::new(|value| value.replay_age_secs = None),
            Box::new(|value| value.replay_age_secs = Some(301)),
            Box::new(|value| value.receiver_age_secs = None),
            Box::new(|value| value.receiver_age_secs = Some(301)),
        ];
        for mutate in mutations {
            let mut candidate = base.clone();
            mutate(&mut candidate);
            assert!(
                reject_failover_candidate(&candidate, evidence_thresholds()).is_some(),
                "mutation should fail closed: {candidate:?}"
            );
        }
    }

    #[test]
    fn evidenced_selector_ranks_lsn_before_lag_and_is_deterministic() {
        let mut newest = complete_evidence("zulu", "2", 200);
        newest.lag_bytes = Some(200_000);
        let lower_lag_but_older = complete_evidence("alpha", "1", 199);
        let candidates = [lower_lag_but_older.clone(), newest.clone()];
        let selected = choose_evidenced_failover_target(&candidates, evidence_thresholds());
        assert_eq!(selected.map(|value| value.stable_id.as_str()), Some("2"));

        newest.name = "same".into();
        newest.stable_id = "b".into();
        let mut tied = newest.clone();
        tied.stable_id = "a".into();
        let candidates = [newest, tied];
        let selected = choose_evidenced_failover_target(&candidates, evidence_thresholds());
        assert_eq!(selected.map(|value| value.stable_id.as_str()), Some("a"));
    }
}
