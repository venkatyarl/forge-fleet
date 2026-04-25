//! Automatic Postgres failover manager.
//!
//! Runs inside `leader_tick` on the fleet leader. Every tick it checks
//! whether the recorded Postgres primary (from `database_replicas` with
//! `role='primary'`) is still reachable. If the primary is both
//! objectively-down (ODOWN in Pulse) **and** its Postgres socket is
//! unreachable, the current fleet leader — when it hosts a local
//! Postgres replica — promotes its own replica and updates
//! `fleet_secrets.postgres_primary_url` so the rest of the fleet can
//! reconnect.
//!
//! ## Intentional conservatism
//!
//! - We never failover on a transient network blip. Primary must be
//!   **both** direct-unreachable and ODOWN from the majority of its
//!   peers' viewpoint (the ODOWN check is delegated to Pulse).
//! - Fencing the old primary is best-effort (`docker stop` via SSH). If
//!   fencing fails we still promote unless `strict_fencing=true`, to
//!   avoid a split-brain requires the old primary to somehow also still
//!   be advertising itself as primary — the new row we write immediately
//!   supersedes it, and subsequent backups/clients look up the URL in
//!   `fleet_secrets`.
//! - The whole path is gated by `FORGEFLEET_DISABLE_AUTO_PG_FAILOVER=true`
//!   and by the caller only invoking us when `i_am_leader == true`.

use std::time::Duration;

use chrono::Utc;
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use ff_db::pg_set_secret;
use ff_pulse::reader::PulseReader;

/// How long to wait before giving up on a direct TCP connect to the
/// current primary's Postgres port.
const PRIMARY_PROBE_TIMEOUT_SECS: u64 = 5;
/// Port the fleet's Postgres listens on (both primary and replica).
const POSTGRES_PORT: u16 = 55432;
/// Replica container name produced by `docker-compose.follower.yml`.
const REPLICA_CONTAINER: &str = "forgefleet-postgres-replica";
/// Primary container name produced by `docker-compose.yml`.
const PRIMARY_CONTAINER: &str = "forgefleet-postgres";
/// `PGDATA` path inside the replica container (matches the follower compose).
const REPLICA_PGDATA: &str = "/var/lib/postgresql/data/pgdata";
/// How long to poll for `pg_is_in_recovery() = false` after issuing
/// `pg_ctl promote`. The replica flips within a few seconds under load,
/// but we give it plenty of headroom.
const PROMOTION_POLL_TIMEOUT_SECS: u64 = 30;
/// Env var that globally disables auto-failover.
pub const DISABLE_ENV: &str = "FORGEFLEET_DISABLE_AUTO_PG_FAILOVER";

/// Errors emitted by [`PostgresFailoverManager`].
#[derive(Debug, thiserror::Error)]
pub enum PgFailoverError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("pulse: {0}")]
    Pulse(#[from] ff_pulse::reader::PulseError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("promotion command failed: {0}")]
    Promote(String),
    #[error("no primary registered in database_replicas")]
    NoPrimary,
    #[error("secrets: {0}")]
    Secrets(String),
}

/// Outcome of a single [`PostgresFailoverManager::check_and_failover`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverOutcome {
    /// Primary is me, or primary is fine, or auto-failover is disabled.
    NoOp,
    /// Primary is ODOWN + Postgres unreachable, but I don't host a replica.
    /// Operator must intervene.
    PrimaryOdownCantPromote,
    /// About to start promotion — caller may log / publish.
    PrimaryOdownPromotingMyReplica,
    /// Successfully promoted my replica to primary.
    Promoted,
    /// Something blocked us (e.g. fencing failed with strict=true).
    Blocked(String),
}

/// Singleton that drives Postgres auto-failover from the fleet leader.
pub struct PostgresFailoverManager {
    pg: PgPool,
    my_computer_id: Uuid,
    /// When true, refuse to promote if fencing the old primary via SSH
    /// fails. Default false — fencing is best-effort and a stopped
    /// container can't participate in split-brain.
    strict_fencing: bool,
}

impl PostgresFailoverManager {
    /// Build a new manager. `strict_fencing` defaults to false.
    pub fn new(pg: PgPool, my_computer_id: Uuid) -> Self {
        Self {
            pg,
            my_computer_id,
            strict_fencing: false,
        }
    }

    /// Enable strict fencing — when true, a failed `docker stop` on the
    /// old primary aborts the promotion.
    pub fn with_strict_fencing(mut self, strict: bool) -> Self {
        self.strict_fencing = strict;
        self
    }

    /// Evaluate the current primary and — if it's demonstrably dead and
    /// I host a replica — promote my replica to primary. Safe to call
    /// every leader tick; normally returns [`FailoverOutcome::NoOp`].
    pub async fn check_and_failover(
        &self,
        pulse: &PulseReader,
    ) -> Result<FailoverOutcome, PgFailoverError> {
        if is_disabled() {
            debug!("pg_failover: disabled via env; skipping");
            return Ok(FailoverOutcome::NoOp);
        }

        // 1) Who is the registered primary?
        let Some(primary) = self.lookup_primary().await? else {
            // No primary row at all — nothing to fail over.
            debug!("pg_failover: no primary row in database_replicas; skipping");
            return Ok(FailoverOutcome::NoOp);
        };

        if primary.computer_id == self.my_computer_id {
            debug!(
                primary = %primary.name,
                "pg_failover: primary is me; no-op"
            );
            return Ok(FailoverOutcome::NoOp);
        }

        // 2) Can we reach the primary's Postgres socket directly?
        let reachable = probe_tcp(
            &primary.primary_ip,
            POSTGRES_PORT,
            PRIMARY_PROBE_TIMEOUT_SECS,
        )
        .await;

        if reachable {
            debug!(
                primary = %primary.name,
                host = %primary.primary_ip,
                reachable = true,
                "pg_failover: primary reachable — no-op"
            );
            return Ok(FailoverOutcome::NoOp);
        }

        // 3) Direct connection failed. Is the primary's computer ODOWN?
        //    If Pulse still says it's alive, treat as a transient network
        //    issue and back off — don't failover.
        let odown = match pulse.is_odown(&primary.name).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    primary = %primary.name,
                    error = %e,
                    "pg_failover: pulse.is_odown lookup failed; refusing to failover"
                );
                return Ok(FailoverOutcome::NoOp);
            }
        };

        if !odown {
            warn!(
                primary = %primary.name,
                host = %primary.primary_ip,
                "pg_failover: primary unreachable but NOT odown in Pulse — \
                 likely transient network, no failover"
            );
            return Ok(FailoverOutcome::NoOp);
        }

        // 4) Do I host a Postgres replica?
        if !self.i_host_a_replica().await? {
            warn!(
                primary = %primary.name,
                "pg_failover: primary odown + unreachable but I don't host a \
                 replica — operator intervention required"
            );
            return Ok(FailoverOutcome::PrimaryOdownCantPromote);
        }

        info!(
            old_primary = %primary.name,
            "pg_failover: primary odown + unreachable — promoting my local replica"
        );

        // 5) Fence + promote.
        match self.promote_local_replica_inner(Some(&primary)).await {
            Ok(()) => {
                info!(
                    old_primary = %primary.name,
                    "pg_failover: local replica promoted to primary"
                );
                Ok(FailoverOutcome::Promoted)
            }
            Err(e) => {
                error!(error = %e, "pg_failover: promotion failed");
                Err(e)
            }
        }
    }

    /// Promote this node's local Postgres replica. Exposed so the
    /// manual `ff fleet db failover` CLI can reuse the same logic.
    pub async fn promote_local_replica(&self) -> Result<(), PgFailoverError> {
        let primary = self.lookup_primary().await?;
        self.promote_local_replica_inner(primary.as_ref()).await
    }

    async fn promote_local_replica_inner(
        &self,
        old_primary: Option<&PrimaryRow>,
    ) -> Result<(), PgFailoverError> {
        // 1) Fence old primary (best-effort unless strict_fencing).
        if let Some(p) = old_primary {
            let fenced = fence_old_primary(&p.ssh_user, &p.primary_ip).await;
            if !fenced {
                let msg = format!(
                    "fencing ssh {}@{} docker stop {} failed",
                    p.ssh_user, p.primary_ip, PRIMARY_CONTAINER
                );
                if self.strict_fencing {
                    return Err(PgFailoverError::Promote(format!(
                        "refusing to promote: {msg}"
                    )));
                }
                warn!(
                    primary = %p.name,
                    "pg_failover: {msg} — continuing (strict_fencing=false)"
                );
            }
        }

        // 2) docker exec pg_ctl promote.
        let out = Command::new("docker")
            .args([
                "exec",
                "-u",
                "postgres",
                REPLICA_CONTAINER,
                "pg_ctl",
                "promote",
                "-D",
                REPLICA_PGDATA,
            ])
            .output()
            .await?;
        if !out.status.success() {
            return Err(PgFailoverError::Promote(format!(
                "pg_ctl promote exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        // 3) Poll pg_is_in_recovery() until false (or timeout).
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(PROMOTION_POLL_TIMEOUT_SECS);
        loop {
            if !is_in_recovery().await.unwrap_or(true) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PgFailoverError::Promote(
                    "timed out waiting for pg_is_in_recovery() = false".into(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // 4) Update database_replicas rows.
        let now = Utc::now();
        // My row becomes primary/running.
        sqlx::query(
            "INSERT INTO database_replicas
                 (computer_id, database_kind, role, status, promoted_at)
             VALUES ($1, 'postgres', 'primary', 'running', $2)
             ON CONFLICT (computer_id, database_kind) DO UPDATE SET
                 role = 'primary',
                 status = 'running',
                 promoted_at = EXCLUDED.promoted_at",
        )
        .bind(self.my_computer_id)
        .bind(now)
        .execute(&self.pg)
        .await?;

        // Old primary's row becomes standby/stopped.
        if let Some(p) = old_primary {
            sqlx::query(
                "UPDATE database_replicas SET role = 'standby', status = 'stopped'
                  WHERE computer_id = $1 AND database_kind = 'postgres'",
            )
            .bind(p.computer_id)
            .execute(&self.pg)
            .await?;
        }

        // 5) Update fleet_secrets.postgres_primary_url.
        let my_ip = self.lookup_my_primary_ip().await?;
        let new_url = format!(
            "postgres://forgefleet:forgefleet@{host}:{port}/forgefleet",
            host = my_ip,
            port = POSTGRES_PORT
        );
        pg_set_secret(
            &self.pg,
            "postgres_primary_url",
            &new_url,
            Some("auto-updated by pg_failover on promotion"),
            Some("pg_failover"),
        )
        .await
        .map_err(|e| PgFailoverError::Secrets(e.to_string()))?;

        // 6) Best-effort NATS event.
        let payload = serde_json::json!({
            "old_primary": old_primary.map(|p| p.name.clone()),
            "new_primary_id": self.my_computer_id,
            "new_url": new_url,
            "promoted_at": now.to_rfc3339(),
        });
        crate::nats_client::publish_json("fleet.events.db.failover", &payload).await;

        Ok(())
    }

    async fn lookup_primary(&self) -> Result<Option<PrimaryRow>, PgFailoverError> {
        let row = sqlx::query(
            "SELECT c.id           AS computer_id,
                    c.name         AS name,
                    c.primary_ip   AS primary_ip,
                    c.ssh_user     AS ssh_user
               FROM database_replicas dr
               JOIN computers c ON c.id = dr.computer_id
              WHERE dr.database_kind = 'postgres'
                AND dr.role = 'primary'
              LIMIT 1",
        )
        .fetch_optional(&self.pg)
        .await?;
        Ok(row.map(|r| PrimaryRow {
            computer_id: r.get("computer_id"),
            name: r.get("name"),
            primary_ip: r.get("primary_ip"),
            ssh_user: r.get("ssh_user"),
        }))
    }

    async fn i_host_a_replica(&self) -> Result<bool, PgFailoverError> {
        let row = sqlx::query(
            "SELECT 1 FROM database_replicas
              WHERE computer_id = $1
                AND database_kind = 'postgres'
                AND role = 'replica'",
        )
        .bind(self.my_computer_id)
        .fetch_optional(&self.pg)
        .await?;
        Ok(row.is_some())
    }

    async fn lookup_my_primary_ip(&self) -> Result<String, PgFailoverError> {
        let ip: Option<String> =
            sqlx::query_scalar("SELECT primary_ip FROM computers WHERE id = $1")
                .bind(self.my_computer_id)
                .fetch_optional(&self.pg)
                .await?;
        Ok(ip.unwrap_or_else(|| "127.0.0.1".to_string()))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PrimaryRow {
    computer_id: Uuid,
    name: String,
    primary_ip: String,
    ssh_user: String,
}

fn is_disabled() -> bool {
    std::env::var(DISABLE_ENV)
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Attempt a TCP connect with a timeout. Returns true on success.
async fn probe_tcp(host: &str, port: u16, timeout_secs: u64) -> bool {
    let addr = format!("{host}:{port}");
    let fut = tokio::net::TcpStream::connect(&addr);
    match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(Ok(_stream)) => true,
        Ok(Err(e)) => {
            debug!(addr = %addr, error = %e, "pg_failover: TCP connect failed");
            false
        }
        Err(_) => {
            debug!(addr = %addr, timeout = timeout_secs, "pg_failover: TCP connect timed out");
            false
        }
    }
}

/// SSH to the old primary and stop its Postgres container. Returns true
/// on success; false on any error (so caller can apply strict policy).
async fn fence_old_primary(ssh_user: &str, host: &str) -> bool {
    let target = format!("{ssh_user}@{host}");
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=5",
            &target,
            "docker",
            "stop",
            PRIMARY_CONTAINER,
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            info!(
                target = %target,
                container = PRIMARY_CONTAINER,
                "pg_failover: fenced old primary via ssh docker stop"
            );
            true
        }
        Ok(o) => {
            warn!(
                target = %target,
                status = %o.status,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "pg_failover: ssh docker stop failed"
            );
            false
        }
        Err(e) => {
            warn!(target = %target, error = %e, "pg_failover: ssh invocation failed");
            false
        }
    }
}

/// Query the local replica container to see if it's still in recovery mode.
async fn is_in_recovery() -> Option<bool> {
    let out = Command::new("docker")
        .args([
            "exec",
            "-u",
            "postgres",
            REPLICA_CONTAINER,
            "psql",
            "-p",
            "55432",
            "-U",
            "forgefleet",
            "-d",
            "forgefleet",
            "-tAc",
            "SELECT pg_is_in_recovery();",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let txt = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
    match txt.as_str() {
        "t" | "true" => Some(true),
        "f" | "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_flag_parses() {
        unsafe {
            std::env::remove_var(DISABLE_ENV);
        }
        assert!(!is_disabled());
        unsafe {
            std::env::set_var(DISABLE_ENV, "true");
        }
        assert!(is_disabled());
        unsafe {
            std::env::set_var(DISABLE_ENV, "1");
        }
        assert!(is_disabled());
        unsafe {
            std::env::set_var(DISABLE_ENV, "no");
        }
        assert!(!is_disabled());
        unsafe {
            std::env::remove_var(DISABLE_ENV);
        }
    }

    #[test]
    fn outcomes_equality() {
        assert_eq!(FailoverOutcome::NoOp, FailoverOutcome::NoOp);
        assert_ne!(
            FailoverOutcome::Promoted,
            FailoverOutcome::PrimaryOdownCantPromote
        );
    }
}
