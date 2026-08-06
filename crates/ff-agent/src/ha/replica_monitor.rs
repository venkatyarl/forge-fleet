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

use std::time::Duration;

use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::pg_failover::{
    ReplicaHost, lsn_lag_bytes, parse_pg_lsn, probe_replica_host, replica_probe_healthy,
};

/// The alert policy seeded by migration V179.
const POLICY_NAME: &str = "postgres_replica_dead";

/// How often the replica health check runs.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);

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

/// Pure transition logic: given the current set of dead replicas and whether
/// an unresolved alert event already exists, decide the alert action.
pub fn decide_alert_action(current_dead: usize, has_unresolved_event: bool) -> AlertAction {
    match (current_dead, has_unresolved_event) {
        (0, true) => AlertAction::Resolve,
        (0, false) => AlertAction::NoOp,
        (_, false) => AlertAction::Fire,
        (_, true) => AlertAction::NoOp,
    }
}

/// Pure: which replicas are dead given probe results.
pub fn dead_from_results(results: &[(ReplicaRow, bool)]) -> Vec<DeadReplica> {
    results
        .iter()
        .filter(|(_, reachable)| !reachable)
        .map(|(r, _)| DeadReplica {
            computer_id: r.computer_id,
            name: r.name.clone(),
            primary_ip: r.primary_ip.clone(),
        })
        .collect()
}

/// The replica health monitor tick. Spawned on every daemon; no-ops on
/// followers via the per-fire leader gate.
pub struct ReplicaMonitorTick {
    pg: PgPool,
    my_name: String,
}

impl ReplicaMonitorTick {
    pub fn new(pg: PgPool, my_name: String) -> Self {
        Self { pg, my_name }
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
            let healthy = probe
                .as_ref()
                .is_some_and(|value| replica_probe_healthy(value, lag_bytes));
            sqlx::query(
                "UPDATE database_replicas
                    SET status = CASE
                            WHEN status='needs_repoint' THEN status
                            WHEN $3 THEN 'running'
                            ELSE 'degraded'
                        END,
                        lag_bytes = $2,
                        last_sync_at = CASE WHEN $3 THEN NOW() ELSE last_sync_at END
                  WHERE computer_id = $1 AND database_kind = 'postgres'
                    AND role = 'replica'",
            )
            .bind(r.computer_id)
            .bind(lag_bytes)
            .bind(healthy)
            .execute(&self.pg)
            .await?;
            debug!(
                replica = %r.name,
                addr = %r.primary_ip,
                healthy,
                lag_bytes,
                "replica_monitor: deep-probed replica"
            );
            results.push((r.clone(), healthy));
        }

        let dead = dead_from_results(&results);

        let policy: Option<(Uuid, String, String)> = match sqlx::query_as(
            "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
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

        let Some((policy_id, severity, channel)) = policy else {
            warn!(
                dead = dead.len(),
                "replica_monitor: {} replica(s) dead but alert policy '{}' missing/disabled — NOT alerting",
                dead.len(),
                POLICY_NAME
            );
            return Ok(dead);
        };

        let has_unresolved = self.has_unresolved_event(policy_id).await?;

        match decide_alert_action(dead.len(), has_unresolved) {
            AlertAction::Fire => self.fire_alert(policy_id, &severity, &channel, &dead).await,
            AlertAction::Resolve => self.resolve_alert(policy_id).await,
            AlertAction::NoOp => {
                if dead.is_empty() {
                    debug!(
                        checked = replicas.len(),
                        "replica_monitor: all replicas reachable"
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
            .map(|d| format!("{} ({})", d.name, d.primary_ip))
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

    #[test]
    fn decide_alert_action_transitions() {
        // No dead, no unresolved -> NoOp
        assert_eq!(decide_alert_action(0, false), AlertAction::NoOp);
        // No dead, unresolved -> Resolve
        assert_eq!(decide_alert_action(0, true), AlertAction::Resolve);
        // Dead, no unresolved -> Fire
        assert_eq!(decide_alert_action(2, false), AlertAction::Fire);
        // Dead, unresolved -> NoOp (already firing)
        assert_eq!(decide_alert_action(2, true), AlertAction::NoOp);
    }

    #[test]
    fn dead_from_results_empty() {
        assert!(dead_from_results(&[]).is_empty());
    }

    #[test]
    fn dead_from_results_mixed() {
        let r1 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r1".into(),
            primary_ip: "10.0.0.2".into(),
            ssh_user: "r1".into(),
        };
        let r2 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r2".into(),
            primary_ip: "10.0.0.3".into(),
            ssh_user: "r2".into(),
        };
        let results = vec![(r1, true), (r2, false)];
        let dead = dead_from_results(&results);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].name, "r2");
        assert_eq!(dead[0].primary_ip, "10.0.0.3");
    }

    #[test]
    fn dead_from_results_all_healthy() {
        let r1 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r1".into(),
            primary_ip: "10.0.0.2".into(),
            ssh_user: "r1".into(),
        };
        let r2 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r2".into(),
            primary_ip: "10.0.0.3".into(),
            ssh_user: "r2".into(),
        };
        let results = vec![(r1, true), (r2, true)];
        assert!(dead_from_results(&results).is_empty());
    }

    #[test]
    fn dead_from_results_both_dead() {
        let r1 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r1".into(),
            primary_ip: "10.0.0.2".into(),
            ssh_user: "r1".into(),
        };
        let r2 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r2".into(),
            primary_ip: "10.0.0.3".into(),
            ssh_user: "r2".into(),
        };
        let results = vec![(r1, false), (r2, false)];
        let dead = dead_from_results(&results);
        assert_eq!(dead.len(), 2);
    }
}
