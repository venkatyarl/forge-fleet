//! Leader-gated Postgres replica health monitor.
//!
//! Periodically probes every registered Postgres replica through its host and
//! verifies recovery, read-only, streaming and replay freshness. It measures
//! replay lag against the live primary LSN and refreshes the authoritative
//! `lag_bytes` / `last_sync_at` evidence consumed by automatic failover.
//!
//! Motivation: both replicas can die silently while the primary and hosts
//! remain up; Pulse beats continue, so host-death alerts never fire and the
//! failover manager's ODOWN gate never trips.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use sqlx::{PgPool, Row};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::pg_failover::{
    ReplicaHost, ReplicaProbe, lsn_lag_bytes, parse_pg_lsn, probe_replica_host,
    replica_probe_healthy,
};

/// The alert policy seeded by migration V179.
const POLICY_NAME: &str = "postgres_replica_dead";

/// How often the replica health check runs.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Promotion eligibility deliberately uses a much tighter 256 KiB limit. The
/// operational monitor only treats lag as an outage signal when it is grossly
/// excessive; promotion still re-evaluates the strict threshold independently.
const MAX_OPERATIONAL_LAG_BYTES: i64 = 64 * 1024 * 1024;
const MAX_OPERATIONAL_EVIDENCE_AGE_SECS: i64 = 300;
const UNHEALTHY_SAMPLES_TO_DEGRADE: u8 = 3;
const HEALTHY_SAMPLES_TO_RECOVER: u8 = 3;

/// A registered Postgres replica as read from the DB.
#[derive(Debug, Clone)]
pub struct ReplicaRow {
    pub computer_id: Uuid,
    pub name: String,
    pub primary_ip: String,
    pub ssh_user: String,
}

/// A replica that failed the TCP probe.
#[derive(Debug, Clone)]
pub struct DeadReplica {
    pub computer_id: Uuid,
    pub name: String,
    pub primary_ip: String,
    pub reason: ReplicaUnhealthyReason,
    pub lag_bytes: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaUnhealthyReason {
    ProbeFailed,
    NotInRecovery,
    NotReadOnly,
    NotStreaming,
    MissingReplayLsn,
    MissingReplayAge,
    InvalidReplayAge(i64),
    StaleReplay(i64),
    MissingReceiverAge,
    InvalidReceiverAge(i64),
    StaleReceiver(i64),
    MissingLag,
    InvalidLag(i64),
    ExcessiveLag(i64),
}

impl fmt::Display for ReplicaUnhealthyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProbeFailed => write!(f, "probe failed"),
            Self::NotInRecovery => write!(f, "pg_is_in_recovery=false"),
            Self::NotReadOnly => write!(f, "transaction_read_only=false"),
            Self::NotStreaming => write!(f, "wal receiver not streaming"),
            Self::MissingReplayLsn => write!(f, "replay LSN missing or invalid"),
            Self::MissingReplayAge => write!(f, "replay age missing"),
            Self::InvalidReplayAge(age) => write!(f, "replay age is negative ({age}s)"),
            Self::StaleReplay(age) => write!(f, "replay stale ({age}s)"),
            Self::MissingReceiverAge => write!(f, "receiver age missing"),
            Self::InvalidReceiverAge(age) => write!(f, "receiver age is negative ({age}s)"),
            Self::StaleReceiver(age) => write!(f, "receiver stale ({age}s)"),
            Self::MissingLag => write!(f, "primary/replay lag unavailable"),
            Self::InvalidLag(lag) => write!(f, "lag is negative ({lag} bytes)"),
            Self::ExcessiveLag(lag) => write!(f, "lag exceeds operational limit ({lag} bytes)"),
        }
    }
}

fn operational_replica_health(
    probe: Option<&ReplicaProbe>,
    lag_bytes: Option<i64>,
) -> Result<(), ReplicaUnhealthyReason> {
    let probe = probe.ok_or(ReplicaUnhealthyReason::ProbeFailed)?;
    if !probe.in_recovery {
        return Err(ReplicaUnhealthyReason::NotInRecovery);
    }
    if !probe.read_only {
        return Err(ReplicaUnhealthyReason::NotReadOnly);
    }
    if !probe.streaming {
        return Err(ReplicaUnhealthyReason::NotStreaming);
    }
    if probe.replay_lsn.is_none() {
        return Err(ReplicaUnhealthyReason::MissingReplayLsn);
    }
    match probe.replay_age_seconds {
        None => return Err(ReplicaUnhealthyReason::MissingReplayAge),
        Some(age) if age < 0 => return Err(ReplicaUnhealthyReason::InvalidReplayAge(age)),
        Some(age) if age > MAX_OPERATIONAL_EVIDENCE_AGE_SECS => {
            return Err(ReplicaUnhealthyReason::StaleReplay(age));
        }
        Some(_) => {}
    }
    match probe.receiver_age_seconds {
        None => return Err(ReplicaUnhealthyReason::MissingReceiverAge),
        Some(age) if age < 0 => return Err(ReplicaUnhealthyReason::InvalidReceiverAge(age)),
        Some(age) if age > MAX_OPERATIONAL_EVIDENCE_AGE_SECS => {
            return Err(ReplicaUnhealthyReason::StaleReceiver(age));
        }
        Some(_) => {}
    }
    match lag_bytes {
        None => Err(ReplicaUnhealthyReason::MissingLag),
        Some(lag) if lag < 0 => Err(ReplicaUnhealthyReason::InvalidLag(lag)),
        Some(lag) if lag > MAX_OPERATIONAL_LAG_BYTES => {
            Err(ReplicaUnhealthyReason::ExcessiveLag(lag))
        }
        Some(_) => Ok(()),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReplicaHealthState {
    consecutive_healthy: u8,
    consecutive_unhealthy: u8,
    degraded: bool,
    last_failure: Option<ReplicaUnhealthyReason>,
}

impl ReplicaHealthState {
    fn record_sample(&mut self, result: Result<(), ReplicaUnhealthyReason>) {
        match result {
            Ok(()) => {
                self.consecutive_unhealthy = 0;
                self.consecutive_healthy = self.consecutive_healthy.saturating_add(1);
                if self.degraded && self.consecutive_healthy >= HEALTHY_SAMPLES_TO_RECOVER {
                    self.degraded = false;
                    self.last_failure = None;
                }
            }
            Err(reason) => {
                self.consecutive_healthy = 0;
                self.consecutive_unhealthy = self.consecutive_unhealthy.saturating_add(1);
                self.last_failure = Some(reason);
                if !self.degraded && self.consecutive_unhealthy >= UNHEALTHY_SAMPLES_TO_DEGRADE {
                    self.degraded = true;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ReplicaResult {
    row: ReplicaRow,
    healthy: bool,
    reason: Option<ReplicaUnhealthyReason>,
    lag_bytes: Option<i64>,
}

fn prune_health_state(states: &mut HashMap<Uuid, ReplicaHealthState>, active_ids: &HashSet<Uuid>) {
    states.retain(|computer_id, _| active_ids.contains(computer_id));
}

/// What to do with the alert state after evaluating one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertAction {
    /// Fire (or re-fire) the alert for these dead replicas.
    Fire,
    /// Resolve any open alert_event for this policy.
    Resolve,
    /// Nothing changed: either all replicas are healthy and no alert is open,
    /// or replicas are still dead and the alert is already firing.
    NoOp,
}

/// Pure transition logic: given the debounced dead replicas, open-event state,
/// and configured cooldown, decide the alert action.
pub fn decide_alert_action(
    current_dead: usize,
    has_unresolved_event: bool,
    cooldown_active: bool,
) -> AlertAction {
    match (current_dead, has_unresolved_event, cooldown_active) {
        (0, true, _) => AlertAction::Resolve,
        (0, false, _) => AlertAction::NoOp,
        (_, false, false) => AlertAction::Fire,
        (_, false, true) | (_, true, _) => AlertAction::NoOp,
    }
}

/// Pure: which replicas are dead given probe results.
fn dead_from_results(results: &[ReplicaResult]) -> Vec<DeadReplica> {
    results
        .iter()
        .filter(|result| !result.healthy)
        .map(|result| DeadReplica {
            computer_id: result.row.computer_id,
            name: result.row.name.clone(),
            primary_ip: result.row.primary_ip.clone(),
            reason: result
                .reason
                .clone()
                .unwrap_or(ReplicaUnhealthyReason::ProbeFailed),
            lag_bytes: result.lag_bytes,
        })
        .collect()
}

/// The replica health monitor tick. Spawned on every daemon; no-ops on
/// followers via the per-fire leader gate.
pub struct ReplicaMonitorTick {
    pg: PgPool,
    my_name: String,
    health_state: Mutex<HashMap<Uuid, ReplicaHealthState>>,
}

impl ReplicaMonitorTick {
    pub fn new(pg: PgPool, my_name: String) -> Self {
        Self {
            pg,
            my_name,
            health_state: Mutex::new(HashMap::new()),
        }
    }

    /// Are we the live leader right now?
    async fn is_live_leader(&self) -> bool {
        crate::leader_cache::is_current_leader()
    }

    /// List every registered Postgres replica with its host's name and IP.
    async fn list_postgres_replicas(&self) -> Result<Vec<ReplicaRow>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT dr.computer_id,
                    c.name,
                    c.primary_ip,
                    c.ssh_user
               FROM database_replicas dr
               JOIN computers c ON c.id = dr.computer_id
              WHERE dr.database_kind = 'postgres'
                AND dr.role = 'replica'
              ORDER BY c.name",
        )
        .fetch_all(&self.pg)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ReplicaRow {
                computer_id: r.get("computer_id"),
                name: r.get("name"),
                primary_ip: r.get("primary_ip"),
                ssh_user: r.get("ssh_user"),
            })
            .collect())
    }

    /// Run one full health pass: probe all replicas, then fire/resolve the
    /// imperative alert on transition. Returns the list of currently-dead
    /// replicas so callers/tests can assert on the outcome.
    pub async fn run_once(&self) -> Result<Vec<DeadReplica>, sqlx::Error> {
        let replicas = self.list_postgres_replicas().await?;
        let active_ids: HashSet<Uuid> = replicas.iter().map(|r| r.computer_id).collect();
        {
            let mut states = self.health_state.lock().await;
            prune_health_state(&mut states, &active_ids);
        }
        if replicas.is_empty() {
            debug!("replica_monitor: no postgres replicas registered");
            return Ok(Vec::new());
        }

        let mut results = Vec::with_capacity(replicas.len());
        for r in &replicas {
            let host = ReplicaHost {
                name: r.name.clone(),
                primary_ip: r.primary_ip.clone(),
                ssh_user: r.ssh_user.clone(),
            };
            let probe = probe_replica_host(&host, r.name.eq_ignore_ascii_case(&self.my_name)).await;
            // Read the primary LSN after the remote probe. Reading it before
            // the probe creates a race where a healthy replica can legitimately
            // report a later LSN and be misclassified as invalid.
            let primary_lsn = sqlx::query_scalar::<_, String>("SELECT pg_current_wal_lsn()::text")
                .fetch_one(&self.pg)
                .await?;
            let primary_lsn = parse_pg_lsn(primary_lsn.trim());
            if primary_lsn.is_none() {
                warn!(
                    replica = %r.name,
                    "replica_monitor: primary LSN is invalid; evidence fails closed"
                );
            }
            let lag_bytes = primary_lsn
                .zip(probe.as_ref().and_then(|value| value.replay_lsn))
                .and_then(|(primary, replay)| lsn_lag_bytes(primary, replay));
            let operational_health = operational_replica_health(probe.as_ref(), lag_bytes);
            let sample_healthy = operational_health.is_ok();
            // Keep promotion readiness visible, but never use its deliberately
            // strict 256 KiB lag gate as an operational outage signal.
            let promotion_eligible = probe
                .as_ref()
                .is_some_and(|value| replica_probe_healthy(value, lag_bytes));
            let (healthy, reason) = {
                let mut states = self.health_state.lock().await;
                let state = states.entry(r.computer_id).or_default();
                state.record_sample(operational_health);
                (!state.degraded, state.last_failure.clone())
            };
            sqlx::query(
                "UPDATE database_replicas
                    SET status = CASE
                            WHEN status='needs_repoint' THEN status
                            WHEN $3 THEN 'running'
                            ELSE 'degraded'
                        END,
                        lag_bytes = $2,
                        last_sync_at = CASE WHEN $4 THEN NOW() ELSE last_sync_at END
                  WHERE computer_id = $1 AND database_kind = 'postgres'
                    AND role = 'replica'",
            )
            .bind(r.computer_id)
            .bind(lag_bytes)
            .bind(healthy)
            // Never refresh failover freshness evidence from an invalid raw
            // sample, even during the alert's failure debounce window.
            .bind(sample_healthy)
            .execute(&self.pg)
            .await?;
            if sample_healthy {
                debug!(
                    replica = %r.name,
                    addr = %r.primary_ip,
                    operationally_healthy = true,
                    debounced_healthy = healthy,
                    promotion_eligible,
                    lag_bytes,
                    "replica_monitor: deep-probed replica"
                );
            } else {
                warn!(
                    replica = %r.name,
                    addr = %r.primary_ip,
                    operationally_healthy = false,
                    debounced_healthy = healthy,
                    promotion_eligible,
                    lag_bytes,
                    reason = %reason.as_ref().expect("unhealthy sample records a reason"),
                    "replica_monitor: deep probe evidence rejected"
                );
            }
            results.push(ReplicaResult {
                row: r.clone(),
                healthy,
                reason,
                lag_bytes,
            });
        }

        let dead = dead_from_results(&results);

        let policy: Option<(Uuid, String, String, i32)> = match sqlx::query_as(
            "SELECT id, severity, channel, cooldown_secs
               FROM alert_policies
              WHERE name = $1 AND enabled = true",
        )
        .bind(POLICY_NAME)
        .fetch_optional(&self.pg)
        .await
        {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "replica_monitor: failed to load {POLICY_NAME} policy");
                return Ok(dead);
            }
        };

        let Some((policy_id, severity, channel, cooldown_secs)) = policy else {
            warn!(
                dead = dead.len(),
                "replica_monitor: {} replica(s) dead but alert policy '{}' missing/disabled — NOT alerting",
                dead.len(),
                POLICY_NAME
            );
            return Ok(dead);
        };

        let has_unresolved = self.has_unresolved_event(policy_id).await?;
        let cooldown_active = if has_unresolved || dead.is_empty() {
            false
        } else {
            self.cooldown_active(policy_id, cooldown_secs).await?
        };

        match decide_alert_action(dead.len(), has_unresolved, cooldown_active) {
            AlertAction::Fire => self.fire_alert(policy_id, &severity, &channel, &dead).await,
            AlertAction::Resolve => self.resolve_alert(policy_id).await,
            AlertAction::NoOp => {
                if dead.is_empty() {
                    debug!(
                        checked = replicas.len(),
                        "replica_monitor: all replicas reachable"
                    );
                } else if cooldown_active {
                    warn!(
                        dead = dead.len(),
                        cooldown_secs, "replica_monitor: alert suppressed by configured cooldown"
                    );
                } else {
                    debug!(dead = dead.len(), "replica_monitor: alert already firing");
                }
            }
        }

        Ok(dead)
    }

    /// True if an unresolved alert_event for this policy (fleet-wide, so
    /// computer_id IS NULL) already exists.
    async fn has_unresolved_event(&self, policy_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1 FROM alert_events
                 WHERE policy_id = $1
                   AND computer_id IS NULL
                   AND resolved_at IS NULL
            )",
        )
        .bind(policy_id)
        .fetch_one(&self.pg)
        .await
    }

    async fn cooldown_active(
        &self,
        policy_id: Uuid,
        cooldown_secs: i32,
    ) -> Result<bool, sqlx::Error> {
        if cooldown_secs <= 0 {
            return Ok(false);
        }
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1 FROM alert_events
                 WHERE policy_id = $1
                   AND computer_id IS NULL
                   AND fired_at >= NOW() - make_interval(secs => $2)
            )",
        )
        .bind(policy_id)
        .bind(cooldown_secs as f64)
        .fetch_one(&self.pg)
        .await
    }

    /// Fire the `postgres_replica_dead` alert through the seeded policy's
    /// channel, then record the `alert_event` row.
    async fn fire_alert(
        &self,
        policy_id: Uuid,
        severity: &str,
        channel: &str,
        dead: &[DeadReplica],
    ) {
        let detail: Vec<String> = dead
            .iter()
            .map(|d| {
                format!(
                    "{} ({}, reason={}, lag_bytes={})",
                    d.name,
                    d.primary_ip,
                    d.reason,
                    d.lag_bytes
                        .map(|lag| lag.to_string())
                        .unwrap_or_else(|| "unknown".into())
                )
            })
            .collect();
        let message = format!(
            "Postgres replica unhealthy: {} replica(s) failed recovery/streaming/freshness/lag evidence (detected by leader '{}'): {}",
            dead.len(),
            self.my_name,
            detail.join(", ")
        );

        // Dispatch FIRST so the recorded channel_result reflects reality.
        let channel_result =
            crate::alert_evaluator::dispatch_alert(&self.pg, channel, severity, &message).await;

        if let Err(e) = sqlx::query(
            "INSERT INTO alert_events \
                (policy_id, computer_id, value, value_text, message, channel_result) \
             VALUES ($1, NULL, $2, NULL, $3, $4)",
        )
        .bind(policy_id)
        .bind(dead.len() as f64)
        .bind(&message)
        .bind(&channel_result)
        .execute(&self.pg)
        .await
        {
            error!(error = %e, "replica_monitor: failed to record alert_event");
        }

        warn!(
            dead = dead.len(),
            channel = %channel,
            channel_result = %channel_result,
            "replica_monitor: postgres replica dead alert fired"
        );
    }

    /// Resolve any open alert_event for this policy.
    async fn resolve_alert(&self, policy_id: Uuid) {
        match sqlx::query(
            "UPDATE alert_events SET resolved_at = NOW()
              WHERE policy_id = $1
                AND computer_id IS NULL
                AND resolved_at IS NULL",
        )
        .bind(policy_id)
        .execute(&self.pg)
        .await
        {
            Ok(result) => {
                if result.rows_affected() > 0 {
                    info!("replica_monitor: postgres replica dead alert resolved");
                }
            }
            Err(e) => error!(error = %e, "replica_monitor: failed to resolve alert_event"),
        }
    }

    /// Spawn the 60s check loop. Leadership is gated inside the loop on every
    /// fire (NOT at spawn), so this is safe to start on every daemon.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CHECK_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_live_leader().await {
                            // A future leadership acquisition must build fresh
                            // consecutive evidence instead of inheriting a
                            // stale pre-handoff streak.
                            self.health_state.lock().await.clear();
                            continue;
                        }
                        match self.run_once().await {
                            Ok(dead) => {
                                if dead.is_empty() {
                                    debug!("replica_monitor: all replicas reachable");
                                } else {
                                    warn!(dead = dead.len(), "replica_monitor: dead replicas detected");
                                }
                            }
                            Err(e) => warn!(error = %e, "replica_monitor: check failed"),
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("replica_monitor tick loop stopped");
                            break;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_probe() -> ReplicaProbe {
        ReplicaProbe {
            in_recovery: true,
            read_only: true,
            streaming: true,
            replay_lsn: Some(1),
            replay_lsn_text: Some("0/1".into()),
            replay_age_seconds: Some(0),
            receiver_age_seconds: Some(0),
        }
    }

    fn row(id: Uuid, name: &str) -> ReplicaRow {
        ReplicaRow {
            computer_id: id,
            name: name.into(),
            primary_ip: "10.0.0.2".into(),
            ssh_user: name.into(),
        }
    }

    #[test]
    fn decide_alert_action_transitions() {
        assert_eq!(decide_alert_action(0, false, false), AlertAction::NoOp);
        assert_eq!(decide_alert_action(0, true, false), AlertAction::Resolve);
        assert_eq!(decide_alert_action(2, false, false), AlertAction::Fire);
        assert_eq!(decide_alert_action(2, true, false), AlertAction::NoOp);
        assert_eq!(decide_alert_action(2, false, true), AlertAction::NoOp);
    }

    #[test]
    fn operational_health_accepts_async_burst_above_promotion_limit() {
        // Deliberately above the 256 KiB promotion limit but below the
        // separate 64 MiB operational outage limit.
        assert_eq!(
            operational_replica_health(Some(&healthy_probe()), Some(300 * 1024)),
            Ok(())
        );
    }

    #[test]
    fn operational_health_rejects_each_incomplete_or_unsafe_evidence_class() {
        assert_eq!(
            operational_replica_health(None, Some(0)),
            Err(ReplicaUnhealthyReason::ProbeFailed)
        );

        let mut probe = healthy_probe();
        probe.in_recovery = false;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::NotInRecovery)
        );
        let mut probe = healthy_probe();
        probe.read_only = false;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::NotReadOnly)
        );
        let mut probe = healthy_probe();
        probe.streaming = false;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::NotStreaming)
        );
        let mut probe = healthy_probe();
        probe.replay_lsn = None;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::MissingReplayLsn)
        );
        let mut probe = healthy_probe();
        probe.replay_age_seconds = None;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::MissingReplayAge)
        );
        let mut probe = healthy_probe();
        probe.replay_age_seconds = Some(-1);
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::InvalidReplayAge(-1))
        );
        let mut probe = healthy_probe();
        probe.replay_age_seconds = Some(301);
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::StaleReplay(301))
        );
        let mut probe = healthy_probe();
        probe.receiver_age_seconds = None;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::MissingReceiverAge)
        );
        let mut probe = healthy_probe();
        probe.receiver_age_seconds = Some(-1);
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::InvalidReceiverAge(-1))
        );
        let mut probe = healthy_probe();
        probe.receiver_age_seconds = Some(301);
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::StaleReceiver(301))
        );
        assert_eq!(
            operational_replica_health(Some(&healthy_probe()), None),
            Err(ReplicaUnhealthyReason::MissingLag)
        );
        assert_eq!(
            operational_replica_health(Some(&healthy_probe()), Some(-1)),
            Err(ReplicaUnhealthyReason::InvalidLag(-1))
        );
        assert_eq!(
            operational_replica_health(Some(&healthy_probe()), Some(MAX_OPERATIONAL_LAG_BYTES + 1)),
            Err(ReplicaUnhealthyReason::ExcessiveLag(
                MAX_OPERATIONAL_LAG_BYTES + 1
            ))
        );
    }

    #[test]
    fn hysteresis_requires_three_failures_and_three_recoveries() {
        let mut state = ReplicaHealthState::default();
        for _ in 0..2 {
            state.record_sample(Err(ReplicaUnhealthyReason::ProbeFailed));
            assert!(!state.degraded);
        }
        state.record_sample(Err(ReplicaUnhealthyReason::ProbeFailed));
        assert!(state.degraded);

        for _ in 0..2 {
            state.record_sample(Ok(()));
            assert!(state.degraded);
        }
        state.record_sample(Ok(()));
        assert!(!state.degraded);
        assert_eq!(state.last_failure, None);
    }

    #[test]
    fn opposite_sample_resets_the_pending_streak() {
        let mut state = ReplicaHealthState::default();
        state.record_sample(Err(ReplicaUnhealthyReason::ProbeFailed));
        state.record_sample(Err(ReplicaUnhealthyReason::ProbeFailed));
        state.record_sample(Ok(()));
        state.record_sample(Err(ReplicaUnhealthyReason::ProbeFailed));
        assert_eq!(state.consecutive_unhealthy, 1);
        assert!(!state.degraded);
    }

    #[test]
    fn dead_result_preserves_reason_and_lag() {
        let id = Uuid::new_v4();
        let results = vec![ReplicaResult {
            row: row(id, "duncan"),
            healthy: false,
            reason: Some(ReplicaUnhealthyReason::ExcessiveLag(70_000_000)),
            lag_bytes: Some(70_000_000),
        }];
        let dead = dead_from_results(&results);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].computer_id, id);
        assert_eq!(
            dead[0].reason,
            ReplicaUnhealthyReason::ExcessiveLag(70_000_000)
        );
        assert_eq!(dead[0].lag_bytes, Some(70_000_000));
    }

    #[test]
    fn state_pruning_keeps_only_registered_replica_ids() {
        let keep = Uuid::new_v4();
        let remove = Uuid::new_v4();
        let mut states = HashMap::from([
            (keep, ReplicaHealthState::default()),
            (remove, ReplicaHealthState::default()),
        ]);
        prune_health_state(&mut states, &HashSet::from([keep]));
        assert_eq!(states.len(), 1);
        assert!(states.contains_key(&keep));
    }
}
