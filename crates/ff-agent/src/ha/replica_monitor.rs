//! Leader-gated Postgres replica health monitor.
//!
//! Periodically probes every registered Postgres replica (rows in
//! `database_replicas` with `database_kind='postgres' AND role='replica'`)
//! via TCP connect to port 55432. When one or more replicas are unreachable,
//! fires the `postgres_replica_dead` imperative alert. Resolves the alert when
//! all replicas are reachable again.
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

/// The alert policy seeded by migration V179.
const POLICY_NAME: &str = "postgres_replica_dead";

/// Port the fleet's Postgres listens on (both primary and replica).
const POSTGRES_PORT: u16 = 55432;

/// How long to wait before giving up on a TCP connect to a replica.
const PROBE_TIMEOUT_SECS: u64 = 5;

/// How often the replica health check runs.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// A registered Postgres replica as read from the DB.
#[derive(Debug, Clone)]
pub struct ReplicaRow {
    pub computer_id: Uuid,
    pub name: String,
    pub primary_ip: String,
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
                    c.primary_ip
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
            let reachable =
                probe_replica_tcp(&r.primary_ip, POSTGRES_PORT, PROBE_TIMEOUT_SECS).await;
            debug!(
                replica = %r.name,
                addr = %r.primary_ip,
                reachable,
                "replica_monitor: probed replica"
            );
            results.push((r.clone(), reachable));
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
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM alert_events
              WHERE policy_id = $1
                AND computer_id IS NULL
                AND resolved_at IS NULL
              LIMIT 1",
        )
        .bind(policy_id)
        .fetch_optional(&self.pg)
        .await?;
        Ok(row.is_some())
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
            "Postgres replica death: {} replica(s) unreachable (detected by leader '{}'): {}",
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

/// Attempt a TCP connect with a timeout. Returns true on success.
async fn probe_replica_tcp(host: &str, port: u16, timeout_secs: u64) -> bool {
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_stream)) => true,
        Ok(Err(e)) => {
            debug!(addr = %addr, error = %e, "replica_monitor: TCP connect failed");
            false
        }
        Err(_) => {
            debug!(addr = %addr, timeout = timeout_secs, "replica_monitor: TCP connect timed out");
            false
        }
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
        };
        let r2 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r2".into(),
            primary_ip: "10.0.0.3".into(),
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
        };
        let r2 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r2".into(),
            primary_ip: "10.0.0.3".into(),
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
        };
        let r2 = ReplicaRow {
            computer_id: Uuid::nil(),
            name: "r2".into(),
            primary_ip: "10.0.0.3".into(),
        };
        let results = vec![(r1, false), (r2, false)];
        let dead = dead_from_results(&results);
        assert_eq!(dead.len(), 2);
    }
}
