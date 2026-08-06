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

/// Telegram accepts at most 4096 Unicode scalar values. Keep the body well
/// below that boundary so dispatch can add `[severity] ` and the largest
/// repeat-summary suffix without making a valid alert unsendable.
const ALERT_MESSAGE_MAX_CHARS: usize = 3_500;
const ALERT_LEADER_MAX_CHARS: usize = 96;
const ALERT_NAME_MAX_CHARS: usize = 96;
const ALERT_IP_MAX_CHARS: usize = 96;
const ALERT_REASON_MAX_CHARS: usize = 192;
const ALERT_DETAIL_MAX_CHARS: usize = 448;

/// A registered Postgres replica as read from the DB.
#[derive(Debug, Clone)]
pub struct ReplicaRow {
    pub computer_id: Uuid,
    pub name: String,
    pub primary_ip: String,
    pub ssh_user: String,
    pub status: String,
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
    PersistedDegraded,
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
            Self::PersistedDegraded => write!(f, "persisted degraded state"),
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

impl ReplicaUnhealthyReason {
    /// Unexpected promotion or writeability can indicate split-brain, so it
    /// must not wait for the ordinary transient-failure debounce window.
    fn requires_immediate_degrade(&self) -> bool {
        matches!(self, Self::NotInRecovery | Self::NotReadOnly)
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
    match probe.receiver_age_seconds {
        None => return Err(ReplicaUnhealthyReason::MissingReceiverAge),
        Some(age) if age < 0 => return Err(ReplicaUnhealthyReason::InvalidReceiverAge(age)),
        Some(age) if age > MAX_OPERATIONAL_EVIDENCE_AGE_SECS => {
            return Err(ReplicaUnhealthyReason::StaleReceiver(age));
        }
        Some(_) => {}
    }
    let lag = match lag_bytes {
        None => return Err(ReplicaUnhealthyReason::MissingLag),
        Some(lag) if lag < 0 => return Err(ReplicaUnhealthyReason::InvalidLag(lag)),
        Some(lag) if lag > MAX_OPERATIONAL_LAG_BYTES => {
            return Err(ReplicaUnhealthyReason::ExcessiveLag(lag));
        }
        Some(lag) => lag,
    };

    match probe.replay_age_seconds {
        Some(age) if age < 0 => Err(ReplicaUnhealthyReason::InvalidReplayAge(age)),
        // pg_last_xact_replay_timestamp tracks the last replayed transaction,
        // not WAL-receiver liveness. On an idle primary it legitimately ages
        // (or can be NULL after restart). Exact zero byte lag plus a fresh
        // streaming receiver is stronger operational evidence of catch-up.
        None if lag == 0 => Ok(()),
        Some(age) if age > MAX_OPERATIONAL_EVIDENCE_AGE_SECS && lag == 0 => Ok(()),
        None => Err(ReplicaUnhealthyReason::MissingReplayAge),
        Some(age) if age > MAX_OPERATIONAL_EVIDENCE_AGE_SECS => {
            Err(ReplicaUnhealthyReason::StaleReplay(age))
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
    fn from_persisted_status(status: &str) -> Self {
        if status == "degraded" {
            Self {
                degraded: true,
                last_failure: Some(ReplicaUnhealthyReason::PersistedDegraded),
                ..Self::default()
            }
        } else {
            Self::default()
        }
    }

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
                let immediate = reason.requires_immediate_degrade();
                self.last_failure = Some(reason);
                if !self.degraded
                    && (immediate || self.consecutive_unhealthy >= UNHEALTHY_SAMPLES_TO_DEGRADE)
                {
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

fn truncate_scalars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut bounded: String = value.chars().take(max_chars - 1).collect();
    bounded.push('…');
    bounded
}

fn render_replica_detail(name: &str, primary_ip: &str, reason: &str, lag: Option<i64>) -> String {
    let name = truncate_scalars(name, ALERT_NAME_MAX_CHARS);
    let primary_ip = truncate_scalars(primary_ip, ALERT_IP_MAX_CHARS);
    let reason = truncate_scalars(reason, ALERT_REASON_MAX_CHARS);
    let lag = lag
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".into());
    let detail = format!("{name} ({primary_ip}, reason={reason}, lag_bytes={lag})");
    // The component limits above keep all fixed reason/lag evidence inside
    // this bound. This final per-detail cap is defense-in-depth if formatting
    // changes later.
    truncate_scalars(&detail, ALERT_DETAIL_MAX_CHARS)
}

fn omitted_replicas_suffix(omitted: usize) -> String {
    format!("… (+{omitted} more replicas)")
}

/// Build a deterministic, Telegram-safe body and return how many sorted rows
/// were omitted. Keeping the count alongside construction makes truncation
/// tests exact rather than inferring it from the rendered string.
fn bounded_alert_message(leader_name: &str, dead: &[DeadReplica]) -> (String, usize) {
    let leader_name = truncate_scalars(leader_name, ALERT_LEADER_MAX_CHARS);
    let mut message = format!(
        "Postgres replica unhealthy: {} replica(s) failed recovery/streaming/freshness/lag evidence (detected by leader '{}'): {}",
        dead.len(),
        leader_name,
        if dead.is_empty() { "none" } else { "" }
    );

    let mut ordered: Vec<&DeadReplica> = dead.iter().collect();
    ordered.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.primary_ip.cmp(&right.primary_ip))
            .then_with(|| left.computer_id.cmp(&right.computer_id))
    });

    let mut retained = 0usize;
    for replica in ordered {
        let detail = render_replica_detail(
            &replica.name,
            &replica.primary_ip,
            &replica.reason.to_string(),
            replica.lag_bytes,
        );
        let separator = if retained == 0 { "" } else { ", " };
        let omitted_after = dead.len() - retained - 1;
        let reserved_suffix = if omitted_after == 0 {
            String::new()
        } else {
            format!("; {}", omitted_replicas_suffix(omitted_after))
        };
        let candidate_chars = message.chars().count()
            + separator.chars().count()
            + detail.chars().count()
            + reserved_suffix.chars().count();
        if candidate_chars > ALERT_MESSAGE_MAX_CHARS {
            break;
        }
        message.push_str(separator);
        message.push_str(&detail);
        retained += 1;
    }

    let omitted = dead.len() - retained;
    if omitted > 0 {
        message.push_str("; ");
        message.push_str(&omitted_replicas_suffix(omitted));
    }

    // This must be a no-op under the budgeting above, but keeps this call site
    // safe if a future edit changes a constant or fixed prefix. Scalar-based
    // truncation cannot split UTF-8.
    (truncate_scalars(&message, ALERT_MESSAGE_MAX_CHARS), omitted)
}

fn alert_message(leader_name: &str, dead: &[DeadReplica]) -> String {
    bounded_alert_message(leader_name, dead).0
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
                    dr.status,
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
                status: r.get("status"),
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
                let state = states
                    .entry(r.computer_id)
                    .or_insert_with(|| ReplicaHealthState::from_persisted_status(&r.status));
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
        let message = alert_message(&self.my_name, dead);

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
            status: "running".into(),
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
    fn operational_health_accepts_idle_zero_lag_with_old_or_missing_replay_timestamp() {
        let mut probe = healthy_probe();
        probe.replay_age_seconds = Some(MAX_OPERATIONAL_EVIDENCE_AGE_SECS + 1);
        assert_eq!(operational_replica_health(Some(&probe), Some(0)), Ok(()));

        probe.replay_age_seconds = None;
        assert_eq!(operational_replica_health(Some(&probe), Some(0)), Ok(()));

        probe.replay_age_seconds = Some(-1);
        assert_eq!(
            operational_replica_health(Some(&probe), Some(0)),
            Err(ReplicaUnhealthyReason::InvalidReplayAge(-1))
        );
    }

    #[test]
    fn operational_health_requires_fresh_replay_timestamp_when_lagging() {
        let mut probe = healthy_probe();
        probe.replay_age_seconds = Some(MAX_OPERATIONAL_EVIDENCE_AGE_SECS + 1);
        assert_eq!(
            operational_replica_health(Some(&probe), Some(1)),
            Err(ReplicaUnhealthyReason::StaleReplay(
                MAX_OPERATIONAL_EVIDENCE_AGE_SECS + 1
            ))
        );

        probe.replay_age_seconds = None;
        assert_eq!(
            operational_replica_health(Some(&probe), Some(1)),
            Err(ReplicaUnhealthyReason::MissingReplayAge)
        );
    }

    #[test]
    fn strict_promotion_gate_remains_tighter_and_replay_strict() {
        let mut probe = healthy_probe();
        assert!(replica_probe_healthy(&probe, Some(256 * 1024)));
        assert!(!replica_probe_healthy(&probe, Some(256 * 1024 + 1)));

        probe.replay_age_seconds = Some(MAX_OPERATIONAL_EVIDENCE_AGE_SECS + 1);
        assert!(!replica_probe_healthy(&probe, Some(0)));
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
            operational_replica_health(Some(&probe), Some(1)),
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
            operational_replica_health(Some(&probe), Some(1)),
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
    fn transient_large_lag_does_not_page_but_sustained_large_lag_does() {
        let reason = ReplicaUnhealthyReason::ExcessiveLag(540 * 1024 * 1024);
        let mut state = ReplicaHealthState::default();
        state.record_sample(Err(reason.clone()));
        assert!(!state.degraded);
        state.record_sample(Ok(()));
        assert!(!state.degraded);
        assert_eq!(state.consecutive_unhealthy, 0);

        for _ in 0..UNHEALTHY_SAMPLES_TO_DEGRADE {
            state.record_sample(Err(reason.clone()));
        }
        assert!(state.degraded);
        assert_eq!(state.last_failure, Some(reason));
    }

    #[test]
    fn structural_promotion_or_writeability_degrades_immediately() {
        for reason in [
            ReplicaUnhealthyReason::NotInRecovery,
            ReplicaUnhealthyReason::NotReadOnly,
        ] {
            let mut state = ReplicaHealthState::default();
            state.record_sample(Err(reason.clone()));
            assert!(state.degraded, "{reason} must page without debounce");
            assert_eq!(state.last_failure, Some(reason));
        }

        let mut state = ReplicaHealthState::default();
        state.record_sample(Err(ReplicaUnhealthyReason::NotStreaming));
        assert!(
            !state.degraded,
            "stream reconnects retain the debounce window"
        );
    }

    #[test]
    fn persisted_degraded_state_survives_restart_until_sustained_recovery() {
        let mut state = ReplicaHealthState::from_persisted_status("degraded");
        assert!(state.degraded);
        assert_eq!(
            decide_alert_action(1, true, false),
            AlertAction::NoOp,
            "an open alert remains open after leader restart"
        );

        for _ in 0..HEALTHY_SAMPLES_TO_RECOVER - 1 {
            state.record_sample(Ok(()));
            assert!(state.degraded);
            assert_eq!(decide_alert_action(1, true, false), AlertAction::NoOp);
        }
        state.record_sample(Ok(()));
        assert!(!state.degraded);
        assert_eq!(decide_alert_action(0, true, false), AlertAction::Resolve);
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
    fn alert_message_names_exact_reason_lag_replica_and_leader() {
        let dead = vec![DeadReplica {
            computer_id: Uuid::new_v4(),
            name: "duncan".into(),
            primary_ip: "192.168.5.114".into(),
            reason: ReplicaUnhealthyReason::ExcessiveLag(540 * 1024 * 1024),
            lag_bytes: Some(540 * 1024 * 1024),
        }];
        let message = alert_message("beyonce", &dead);
        assert!(message.contains("duncan (192.168.5.114"));
        assert!(message.contains("reason=lag exceeds operational limit (566231040 bytes)"));
        assert!(message.contains("lag_bytes=566231040"));
        assert!(message.contains("detected by leader 'beyonce'"));
    }

    #[test]
    fn unicode_components_and_detail_are_scalar_bounded_without_losing_lag() {
        let detail = render_replica_detail(
            &"名".repeat(4_000),
            &"址".repeat(4_000),
            &"原".repeat(4_000),
            Some(i64::MIN),
        );
        assert!(detail.chars().count() <= ALERT_DETAIL_MAX_CHARS);
        assert!(detail.contains('…'));
        assert!(detail.ends_with("lag_bytes=-9223372036854775808)"));

        let dead = vec![DeadReplica {
            computer_id: Uuid::from_u128(1),
            name: "名".repeat(4_000),
            primary_ip: "址".repeat(4_000),
            reason: ReplicaUnhealthyReason::ProbeFailed,
            lag_bytes: None,
        }];
        let (message, omitted) = bounded_alert_message(&"领".repeat(4_000), &dead);
        assert_eq!(omitted, 0);
        assert!(message.chars().count() <= ALERT_MESSAGE_MAX_CHARS);
        assert!(message.contains(&"领".repeat(ALERT_LEADER_MAX_CHARS - 1)));
        assert!(!message.contains(&"领".repeat(ALERT_LEADER_MAX_CHARS)));
        assert!(message.contains("reason=probe failed"));
        assert!(message.contains("lag_bytes=unknown"));
    }

    #[test]
    fn many_replicas_are_sorted_and_report_exact_omitted_count() {
        let replicas: Vec<DeadReplica> = (0..40)
            .rev()
            .map(|index| DeadReplica {
                computer_id: Uuid::from_u128(index + 1),
                name: format!("replica-{index:03}-{}", "名".repeat(400)),
                primary_ip: format!("10.0.0.{index}-{}", "址".repeat(400)),
                reason: ReplicaUnhealthyReason::ExcessiveLag(540 * 1024 * 1024),
                lag_bytes: Some(540 * 1024 * 1024),
            })
            .collect();
        let (message, omitted) = bounded_alert_message("beyonce", &replicas);
        let retained = replicas.len() - omitted;

        assert!(omitted > 0);
        assert!(retained > 0);
        assert!(message.chars().count() <= ALERT_MESSAGE_MAX_CHARS);
        assert_eq!(message.matches("lag_bytes=").count(), retained);
        assert!(message.ends_with(&omitted_replicas_suffix(omitted)));
        assert!(message.contains("reason=lag exceeds operational limit"));

        let first = message.find("replica-000-").expect("sorted first replica");
        let second = message.find("replica-001-").expect("sorted second replica");
        assert!(first < second);
    }

    #[test]
    fn replica_message_order_is_stable_across_input_order() {
        let make = |id, name: &str| DeadReplica {
            computer_id: Uuid::from_u128(id),
            name: name.into(),
            primary_ip: format!("10.0.0.{id}"),
            reason: ReplicaUnhealthyReason::NotStreaming,
            lag_bytes: Some(id as i64),
        };
        let forward = vec![make(1, "alpha"), make(2, "beta"), make(3, "gamma")];
        let reverse = vec![make(3, "gamma"), make(2, "beta"), make(1, "alpha")];
        assert_eq!(
            bounded_alert_message("beyonce", &forward),
            bounded_alert_message("beyonce", &reverse)
        );
    }

    #[test]
    fn telegram_dispatch_prefix_and_largest_repeat_suffix_fit_reserved_headroom() {
        let dispatch_prefix = "[critical] ";
        let repeat_suffix = format!("\n(repeated {} times since the previous alert)", i64::MAX);
        assert!(
            ALERT_MESSAGE_MAX_CHARS
                + dispatch_prefix.chars().count()
                + repeat_suffix.chars().count()
                <= 4_096
        );
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
