//! Fail-closed PostgreSQL automatic failover.
//!
//! The leader's ordinary PgPool points at the current primary and is not
//! usable after that primary fails. While authority is healthy we therefore
//! retain a short-lived in-memory topology snapshot. A failover is considered
//! only when the primary is both TCP-unreachable and ODOWN in Pulse. Every
//! registered replica is then independently probed through its host; complete
//! recovery/read-only/streaming/replay evidence is required and candidates are
//! ranked deterministically.
//!
//! Promotion requires successful fencing by default. After promotion the
//! manager connects directly to the new primary and transactionally
//! compare-and-sets the exact old-primary and replica roles together with the
//! DSN of record. Missing or stale evidence always blocks automatic failover.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use ff_pulse::reader::PulseReader;

use super::{
    CandidateRejection, FailoverCandidateEvidence, FailoverEvidenceThresholds, ReplicaRole,
    choose_evidenced_failover_target, handoff,
};

const PRIMARY_PROBE_TIMEOUT_SECS: u64 = 5;
const CONTROL_QUERY_TIMEOUT_SECS: u64 = 5;
const REMOTE_COMMAND_TIMEOUT_SECS: u64 = 20;
const POSTGRES_PORT: u16 = 55432;
const REPLICA_CONTAINER: &str = "forgefleet-postgres-replica";
const REPLICA_PGDATA: &str = "/var/lib/postgresql/data/pgdata";
const FENCE_PRIMARY_COMMAND: &str = r#"found=0
for container in forgefleet-postgres forgefleet-postgres-replica; do
  if docker inspect "$container" >/dev/null 2>&1; then
    found=1
    docker stop "$container" >/dev/null || exit 1
  fi
done
[ "$found" -eq 1 ] || exit 1
command -v ss >/dev/null 2>&1 || exit 1
command -v awk >/dev/null 2>&1 || exit 1
command -v grep >/dev/null 2>&1 || exit 1
! ss -H -ltn | awk '{print $4}' | grep -Eq '(:|])55432$'"#;
const PROMOTION_POLL_TIMEOUT_SECS: u64 = 45;
const MAX_TOPOLOGY_CACHE_AGE_SECS: i64 = 300;
pub(crate) const MAX_LAST_SYNC_AGE_SECS: i64 = 300;
pub(crate) const MAX_REPLAY_AGE_SECS: i64 = 300;
pub(crate) const MAX_RECEIVER_AGE_SECS: i64 = 300;
pub const DISABLE_ENV: &str = "FORGEFLEET_DISABLE_AUTO_PG_FAILOVER";

const REPLICA_PROBE_SQL: &str = r#"SELECT json_build_object(
    'in_recovery', pg_is_in_recovery(),
    'read_only', current_setting('transaction_read_only')::boolean,
    'streaming', EXISTS (
        SELECT 1 FROM pg_stat_wal_receiver WHERE status = 'streaming'
    ),
    'replay_lsn', pg_last_wal_replay_lsn()::text,
    'replay_age_seconds', CASE
        WHEN pg_last_xact_replay_timestamp() IS NULL THEN NULL
        ELSE GREATEST(
            0,
            EXTRACT(EPOCH FROM (
                clock_timestamp() - pg_last_xact_replay_timestamp()
            ))::bigint
        )
    END,
    'receiver_age_seconds', (
        SELECT GREATEST(
            0,
            EXTRACT(EPOCH FROM (
                clock_timestamp() - last_msg_receipt_time
            ))::bigint
        )
        FROM pg_stat_wal_receiver
        WHERE status = 'streaming'
        ORDER BY last_msg_receipt_time DESC
        LIMIT 1
    )
)::text;"#;

const PROMOTED_PRIMARY_PROBE_SQL: &str = "SELECT (NOT pg_is_in_recovery())::text;";

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
    #[error("authority evidence invalid: {0}")]
    Authority(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverOutcome {
    NoOp,
    Promoted {
        target: String,
        estimated_rpo_bytes: i64,
        estimated_rpo_seconds: i64,
    },
    Blocked(String),
}

pub struct PostgresFailoverManager {
    pg: PgPool,
    my_computer_id: Uuid,
    strict_fencing: bool,
    topology_cache: RwLock<Option<TopologySnapshot>>,
    promoted_pg: RwLock<Option<PgPool>>,
    /// Once promotion has been issued, any failure before the authority CAS is
    /// operationally ambiguous. Never attempt a second candidate in-process.
    promotion_in_doubt: AtomicBool,
}

impl PostgresFailoverManager {
    /// Automatic failover is strict-fenced by default. The manual CLI may
    /// explicitly override this only through its existing --force path.
    pub fn new(pg: PgPool, my_computer_id: Uuid) -> Self {
        Self {
            pg,
            my_computer_id,
            strict_fencing: true,
            topology_cache: RwLock::new(None),
            promoted_pg: RwLock::new(None),
            promotion_in_doubt: AtomicBool::new(false),
        }
    }

    pub fn with_strict_fencing(mut self, strict: bool) -> Self {
        self.strict_fencing = strict;
        self
    }

    pub async fn check_and_failover(
        &self,
        pulse: &PulseReader,
    ) -> Result<FailoverOutcome, PgFailoverError> {
        if is_disabled() {
            debug!("pg_failover: disabled via env; skipping");
            return Ok(FailoverOutcome::NoOp);
        }
        if self.promotion_in_doubt.load(Ordering::Acquire) {
            return Ok(FailoverOutcome::Blocked(
                "a prior promotion attempt is in doubt; operator reconciliation required".into(),
            ));
        }

        let topology = match self.topology_for_check().await {
            Ok(topology) => topology,
            Err(error) => {
                warn!(%error, "pg_failover: no complete fresh authority snapshot");
                return Ok(FailoverOutcome::Blocked(error.to_string()));
            }
        };
        let primary = &topology.primary;

        if probe_tcp(
            &primary.primary_ip,
            POSTGRES_PORT,
            PRIMARY_PROBE_TIMEOUT_SECS,
        )
        .await
        {
            debug!(
                primary = %primary.name,
                host = %primary.primary_ip,
                "pg_failover: primary reachable"
            );
            return Ok(FailoverOutcome::NoOp);
        }

        let odown = match pulse.is_odown(&primary.name).await {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    primary = %primary.name,
                    %error,
                    "pg_failover: Pulse authority unavailable; refusing failover"
                );
                return Ok(FailoverOutcome::Blocked(
                    "Pulse ODOWN evidence unavailable".into(),
                ));
            }
        };
        if !odown {
            warn!(
                primary = %primary.name,
                "pg_failover: primary unreachable but not ODOWN; refusing failover"
            );
            return Ok(FailoverOutcome::NoOp);
        }

        let candidate = match self.select_candidate(&topology).await {
            Ok(candidate) => candidate,
            Err(reason) => {
                warn!(
                    primary = %primary.name,
                    %reason,
                    "pg_failover: no eligible replica"
                );
                return Ok(FailoverOutcome::Blocked(reason));
            }
        };

        let estimated_rpo_bytes = candidate.row.lag_bytes.unwrap_or(i64::MAX);
        let estimated_rpo_seconds = candidate.probe.replay_age_seconds.unwrap_or(i64::MAX);
        warn!(
            old_primary = %primary.name,
            candidate = %candidate.row.name,
            estimated_async_rpo_bytes = estimated_rpo_bytes,
            estimated_async_rpo_seconds = estimated_rpo_seconds,
            replay_lsn = candidate.probe.replay_lsn,
            "pg_failover: async replica selected; promotion may lose unreplicated commits"
        );

        match self.promote_candidate(primary, &candidate).await {
            Ok(()) => Ok(FailoverOutcome::Promoted {
                target: candidate.row.name.clone(),
                estimated_rpo_bytes,
                estimated_rpo_seconds,
            }),
            Err(error) => {
                error!(
                    candidate = %candidate.row.name,
                    %error,
                    "pg_failover: promotion failed"
                );
                Err(error)
            }
        }
    }

    /// Manual promotion reuses all evidence, fencing and CAS checks. It is
    /// still local-only because the CLI intentionally runs on its target.
    pub async fn promote_local_replica(&self) -> Result<(), PgFailoverError> {
        if self.promotion_in_doubt.load(Ordering::Acquire) {
            return Err(PgFailoverError::Authority(
                "a prior promotion attempt is in doubt".into(),
            ));
        }
        let topology = self.topology_for_check().await?;
        let candidate = self
            .observe_candidate(
                topology
                    .replicas
                    .iter()
                    .find(|row| row.computer_id == self.my_computer_id)
                    .ok_or_else(|| {
                        PgFailoverError::Authority(
                            "this host has no registered PostgreSQL replica".into(),
                        )
                    })?,
            )
            .await;
        let rejection = reject_observed_candidate(&candidate);
        if let Some(reason) = rejection {
            return Err(PgFailoverError::Authority(format!(
                "local replica evidence rejected: {reason:?}"
            )));
        }
        let selected = SelectedCandidate {
            row: candidate.row,
            probe: candidate.probe.ok_or_else(|| {
                PgFailoverError::Authority("local replica probe returned no evidence".into())
            })?,
        };
        self.promote_candidate(&topology.primary, &selected).await
    }

    async fn control_pool(&self) -> PgPool {
        self.promoted_pg
            .read()
            .await
            .clone()
            .unwrap_or_else(|| self.pg.clone())
    }

    async fn topology_for_check(&self) -> Result<TopologySnapshot, PgFailoverError> {
        let refresh = tokio::time::timeout(
            Duration::from_secs(CONTROL_QUERY_TIMEOUT_SECS),
            self.refresh_topology(),
        )
        .await;
        match refresh {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(error)) => {
                warn!(%error, "pg_failover: live topology refresh failed; considering cache");
                self.fresh_cached_topology().await.ok_or(error)
            }
            Err(_) => self.fresh_cached_topology().await.ok_or_else(|| {
                PgFailoverError::Authority(format!(
                    "topology refresh timed out and no cache newer than {MAX_TOPOLOGY_CACHE_AGE_SECS}s exists"
                ))
            }),
        }
    }

    async fn refresh_topology(&self) -> Result<TopologySnapshot, PgFailoverError> {
        let pool = self.control_pool().await;
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let primary_rows = sqlx::query(
            "SELECT c.id AS computer_id, c.name, c.primary_ip, c.ssh_user,
                    dr.status
               FROM database_replicas dr
               JOIN computers c ON c.id = dr.computer_id
              WHERE dr.database_kind = 'postgres' AND dr.role = 'primary'
              ORDER BY c.id",
        )
        .fetch_all(&mut *tx)
        .await?;
        if primary_rows.len() != 1 {
            return Err(PgFailoverError::Authority(format!(
                "expected exactly one PostgreSQL primary row, found {}",
                primary_rows.len()
            )));
        }
        let primary_row = &primary_rows[0];
        let primary_status: String = primary_row.get("status");
        if primary_status != "running" {
            return Err(PgFailoverError::Authority(format!(
                "registered PostgreSQL primary status is '{primary_status}', not 'running'"
            )));
        }
        let primary = PrimaryRow {
            computer_id: primary_row.get("computer_id"),
            name: primary_row.get("name"),
            primary_ip: primary_row.get("primary_ip"),
            ssh_user: primary_row.get("ssh_user"),
        };

        let replicas = sqlx::query(
            "SELECT dr.computer_id, c.name, c.primary_ip, c.ssh_user,
                    dr.role, dr.status, dr.lag_bytes, dr.last_sync_at
               FROM database_replicas dr
               JOIN computers c ON c.id = dr.computer_id
              WHERE dr.database_kind = 'postgres' AND dr.role = 'replica'
              ORDER BY c.name, c.id",
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| ReplicaRow {
            computer_id: row.get("computer_id"),
            name: row.get("name"),
            primary_ip: row.get("primary_ip"),
            ssh_user: row.get("ssh_user"),
            role: row.get("role"),
            status: row.get("status"),
            lag_bytes: row.get("lag_bytes"),
            last_sync_at: row.get("last_sync_at"),
        })
        .collect();
        tx.commit().await?;

        let snapshot = TopologySnapshot {
            primary,
            replicas,
            collected_at: Utc::now(),
        };
        *self.topology_cache.write().await = Some(snapshot.clone());
        Ok(snapshot)
    }

    async fn fresh_cached_topology(&self) -> Option<TopologySnapshot> {
        let cached = self.topology_cache.read().await.clone()?;
        topology_cache_is_fresh(Utc::now(), cached.collected_at).then_some(cached)
    }

    async fn select_candidate(
        &self,
        topology: &TopologySnapshot,
    ) -> Result<SelectedCandidate, String> {
        if topology.replicas.is_empty() {
            return Err("no registered PostgreSQL replicas".into());
        }
        let mut observed = Vec::with_capacity(topology.replicas.len());
        for row in &topology.replicas {
            observed.push(self.observe_candidate(row).await);
        }
        let evidence: Vec<_> = observed.iter().map(|item| item.evidence.clone()).collect();
        let Some(selected_evidence) =
            choose_evidenced_failover_target(&evidence, evidence_thresholds())
        else {
            let reasons = observed
                .iter()
                .map(|candidate| {
                    format!(
                        "{}={:?}",
                        candidate.row.name,
                        reject_observed_candidate(candidate)
                            .unwrap_or(CandidateRejection::Missing("unknown"))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("all replica evidence rejected: {reasons}"));
        };
        let selected = observed
            .into_iter()
            .find(|item| item.evidence.stable_id == selected_evidence.stable_id)
            .expect("selected evidence originated from observed candidates");
        Ok(SelectedCandidate {
            row: selected.row,
            probe: selected
                .probe
                .expect("eligible candidate always has complete live probe"),
        })
    }

    async fn observe_candidate(&self, row: &ReplicaRow) -> ObservedCandidate {
        let host = row.as_host();
        let probe = probe_replica_host(&host, row.computer_id == self.my_computer_id).await;
        let last_sync_age_secs = row
            .last_sync_at
            .map(|timestamp| Utc::now().signed_duration_since(timestamp).num_seconds());
        let evidence = FailoverCandidateEvidence {
            stable_id: row.computer_id.to_string(),
            name: row.name.clone(),
            role: parse_role(&row.role),
            status: row.status.clone(),
            lag_bytes: row.lag_bytes,
            last_sync_age_secs,
            in_recovery: probe.as_ref().map(|value| value.in_recovery),
            read_only: probe.as_ref().map(|value| value.read_only),
            streaming: probe.as_ref().map(|value| value.streaming),
            replay_lsn: probe.as_ref().and_then(|value| value.replay_lsn),
            replay_age_secs: probe.as_ref().and_then(|value| value.replay_age_seconds),
            receiver_age_secs: probe.as_ref().and_then(|value| value.receiver_age_seconds),
        };
        ObservedCandidate {
            row: row.clone(),
            probe,
            evidence,
        }
    }

    async fn promote_candidate(
        &self,
        old_primary: &PrimaryRow,
        candidate: &SelectedCandidate,
    ) -> Result<(), PgFailoverError> {
        let observed = self.observe_candidate(&candidate.row).await;
        if let Some(reason) = reject_observed_candidate(&observed) {
            return Err(PgFailoverError::Authority(format!(
                "candidate evidence changed before promotion: {reason:?}"
            )));
        }
        let latest_probe = observed
            .probe
            .ok_or_else(|| PgFailoverError::Authority("candidate probe disappeared".into()))?;
        if latest_probe.replay_lsn < candidate.probe.replay_lsn {
            return Err(PgFailoverError::Authority(
                "candidate replay LSN moved backwards".into(),
            ));
        }

        let standby_pool = connect_candidate_pool(&candidate.row).await?;
        let is_standby: bool = sqlx::query_scalar("SELECT pg_is_in_recovery()")
            .fetch_one(&standby_pool)
            .await?;
        standby_pool.close().await;
        if !is_standby {
            return Err(PgFailoverError::Authority(
                "candidate direct connection is not in recovery".into(),
            ));
        }

        let fenced = fence_old_primary(&old_primary.ssh_user, &old_primary.primary_ip).await;
        if !fenced && self.strict_fencing {
            return Err(PgFailoverError::Promote(format!(
                "refusing promotion: failed to fence {}",
                old_primary.name
            )));
        }
        if !fenced {
            warn!(
                old_primary = %old_primary.name,
                "pg_failover: proceeding without fencing due to explicit manual override"
            );
        }

        self.promotion_in_doubt
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                PgFailoverError::Authority("another promotion attempt is active or in doubt".into())
            })?;
        promote_host(
            &candidate.row.as_host(),
            candidate.row.computer_id == self.my_computer_id,
        )
        .await?;
        wait_for_promoted_primary(
            &candidate.row.as_host(),
            candidate.row.computer_id == self.my_computer_id,
        )
        .await?;

        let new_pool = connect_candidate_pool(&candidate.row).await?;
        commit_authority_cas(
            &new_pool,
            old_primary,
            candidate,
            latest_probe.replay_age_seconds.unwrap_or(i64::MAX),
        )
        .await?;
        *self.promoted_pg.write().await = Some(new_pool.clone());
        self.promotion_in_doubt.store(false, Ordering::Release);

        let payload = serde_json::json!({
            "old_primary": old_primary.name,
            "new_primary": candidate.row.name,
            "new_primary_id": candidate.row.computer_id,
            "new_primary_host": candidate.row.primary_ip,
            "estimated_async_rpo_bytes": candidate.row.lag_bytes,
            "estimated_async_rpo_seconds": latest_probe.replay_age_seconds,
            "replay_lsn": latest_probe.replay_lsn_text,
            "promoted_at": Utc::now().to_rfc3339(),
            "fenced": fenced,
        });
        crate::nats_client::publish_json("fleet.events.db.failover", &payload).await;
        info!(
            old_primary = %old_primary.name,
            new_primary = %candidate.row.name,
            "pg_failover: remote-capable promotion and authority CAS complete"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TopologySnapshot {
    primary: PrimaryRow,
    replicas: Vec<ReplicaRow>,
    collected_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct PrimaryRow {
    computer_id: Uuid,
    name: String,
    primary_ip: String,
    ssh_user: String,
}

#[derive(Debug, Clone)]
struct ReplicaRow {
    computer_id: Uuid,
    name: String,
    primary_ip: String,
    ssh_user: String,
    role: String,
    status: String,
    lag_bytes: Option<i64>,
    last_sync_at: Option<DateTime<Utc>>,
}

impl ReplicaRow {
    fn as_host(&self) -> ReplicaHost {
        ReplicaHost {
            name: self.name.clone(),
            primary_ip: self.primary_ip.clone(),
            ssh_user: self.ssh_user.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct ObservedCandidate {
    row: ReplicaRow,
    probe: Option<ReplicaProbe>,
    evidence: FailoverCandidateEvidence,
}

#[derive(Debug, Clone)]
struct SelectedCandidate {
    row: ReplicaRow,
    probe: ReplicaProbe,
}

#[derive(Debug, Clone)]
pub(crate) struct ReplicaHost {
    pub(crate) name: String,
    pub(crate) primary_ip: String,
    pub(crate) ssh_user: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplicaProbe {
    pub(crate) in_recovery: bool,
    pub(crate) read_only: bool,
    pub(crate) streaming: bool,
    pub(crate) replay_lsn: Option<u64>,
    pub(crate) replay_lsn_text: Option<String>,
    pub(crate) replay_age_seconds: Option<i64>,
    pub(crate) receiver_age_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawReplicaProbe {
    in_recovery: bool,
    read_only: bool,
    streaming: bool,
    replay_lsn: Option<String>,
    replay_age_seconds: Option<i64>,
    receiver_age_seconds: Option<i64>,
}

fn evidence_thresholds() -> FailoverEvidenceThresholds {
    FailoverEvidenceThresholds {
        max_lag_bytes: handoff::MAX_SAFE_LAG_BYTES,
        max_last_sync_age_secs: MAX_LAST_SYNC_AGE_SECS,
        max_replay_age_secs: MAX_REPLAY_AGE_SECS,
        max_receiver_age_secs: MAX_RECEIVER_AGE_SECS,
    }
}

fn reject_observed_candidate(candidate: &ObservedCandidate) -> Option<CandidateRejection> {
    super::reject_failover_candidate(&candidate.evidence, evidence_thresholds())
}

fn parse_role(role: &str) -> ReplicaRole {
    match role {
        "replica" => ReplicaRole::Replica,
        "primary" => ReplicaRole::Primary,
        _ => ReplicaRole::Standby,
    }
}

fn topology_cache_is_fresh(now: DateTime<Utc>, collected_at: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(collected_at).num_seconds();
    (0..=MAX_TOPOLOGY_CACHE_AGE_SECS).contains(&age)
}

pub(crate) fn parse_pg_lsn(value: &str) -> Option<u64> {
    let (high, low) = value.trim().split_once('/')?;
    let high = u64::from_str_radix(high, 16).ok()?;
    let low = u64::from_str_radix(low, 16).ok()?;
    (high <= u32::MAX as u64 && low <= u32::MAX as u64).then_some((high << 32) | low)
}

pub(crate) fn lsn_lag_bytes(primary_lsn: u64, replay_lsn: u64) -> Option<i64> {
    let distance = primary_lsn.checked_sub(replay_lsn)?;
    i64::try_from(distance).ok()
}

fn parse_replica_probe(output: &str) -> Option<ReplicaProbe> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with('{'))?;
    let raw: RawReplicaProbe = serde_json::from_str(line.trim()).ok()?;
    let replay_lsn = raw.replay_lsn.as_deref().and_then(parse_pg_lsn);
    Some(ReplicaProbe {
        in_recovery: raw.in_recovery,
        read_only: raw.read_only,
        streaming: raw.streaming,
        replay_lsn,
        replay_lsn_text: raw.replay_lsn,
        replay_age_seconds: raw.replay_age_seconds,
        receiver_age_seconds: raw.receiver_age_seconds,
    })
}

pub(crate) fn replica_probe_healthy(probe: &ReplicaProbe, lag_bytes: Option<i64>) -> bool {
    let evidence = FailoverCandidateEvidence {
        stable_id: "monitor".into(),
        name: "monitor".into(),
        role: ReplicaRole::Replica,
        status: "running".into(),
        lag_bytes,
        last_sync_age_secs: Some(0),
        in_recovery: Some(probe.in_recovery),
        read_only: Some(probe.read_only),
        streaming: Some(probe.streaming),
        replay_lsn: probe.replay_lsn,
        replay_age_secs: probe.replay_age_seconds,
        receiver_age_secs: probe.receiver_age_seconds,
    };
    super::reject_failover_candidate(&evidence, evidence_thresholds()).is_none()
}

pub(crate) async fn probe_replica_host(host: &ReplicaHost, local: bool) -> Option<ReplicaProbe> {
    let output = if local {
        let port = POSTGRES_PORT.to_string();
        let output = tokio::time::timeout(
            Duration::from_secs(REMOTE_COMMAND_TIMEOUT_SECS),
            Command::new("docker")
                .args([
                    "exec",
                    "-u",
                    "postgres",
                    REPLICA_CONTAINER,
                    "psql",
                    "-XAt",
                    "-p",
                    &port,
                    "-U",
                    "forgefleet",
                    "-d",
                    "forgefleet",
                    "-c",
                    REPLICA_PROBE_SQL,
                ])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())?
    } else {
        run_remote_command(host, &replica_probe_remote_command())
            .await
            .ok()?
    };
    parse_replica_probe(&output)
}

fn replica_probe_remote_command() -> String {
    format!(
        "docker exec -u postgres {REPLICA_CONTAINER} psql -XAt -p {POSTGRES_PORT} -U forgefleet -d forgefleet -c \"{REPLICA_PROBE_SQL}\""
    )
}

fn promoted_primary_probe_remote_command() -> String {
    format!(
        "docker exec -u postgres {REPLICA_CONTAINER} psql -XAt -p {POSTGRES_PORT} -U forgefleet -d forgefleet -c \"{PROMOTED_PRIMARY_PROBE_SQL}\""
    )
}

fn promote_remote_command() -> String {
    format!("docker exec -u postgres {REPLICA_CONTAINER} pg_ctl promote -D {REPLICA_PGDATA}")
}

async fn run_remote_command(
    host: &ReplicaHost,
    remote_command: &str,
) -> Result<String, PgFailoverError> {
    let target = format!("{}@{}", host.ssh_user, host.primary_ip);
    let output = tokio::time::timeout(
        Duration::from_secs(REMOTE_COMMAND_TIMEOUT_SECS),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=5",
                &target,
                remote_command,
            ])
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| PgFailoverError::Promote(format!("SSH command timed out on {}", host.name)))??;
    if !output.status.success() {
        return Err(PgFailoverError::Promote(format!(
            "SSH command failed on {}: {}",
            host.name,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn promote_host(host: &ReplicaHost, local: bool) -> Result<(), PgFailoverError> {
    if !local {
        run_remote_command(host, &promote_remote_command()).await?;
        return Ok(());
    }
    let output = Command::new("docker")
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
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(PgFailoverError::Promote(format!(
            "pg_ctl promote exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

async fn wait_for_promoted_primary(host: &ReplicaHost, local: bool) -> Result<(), PgFailoverError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(PROMOTION_POLL_TIMEOUT_SECS);
    loop {
        let output = if local {
            let port = POSTGRES_PORT.to_string();
            Command::new("docker")
                .args([
                    "exec",
                    "-u",
                    "postgres",
                    REPLICA_CONTAINER,
                    "psql",
                    "-XAt",
                    "-p",
                    &port,
                    "-U",
                    "forgefleet",
                    "-d",
                    "forgefleet",
                    "-c",
                    PROMOTED_PRIMARY_PROBE_SQL,
                ])
                .stdin(Stdio::null())
                .output()
                .await
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            run_remote_command(host, &promoted_primary_probe_remote_command())
                .await
                .ok()
        };
        if output
            .as_deref()
            .is_some_and(|value| value.trim() == "true")
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(PgFailoverError::Promote(format!(
                "timed out waiting for {} to leave recovery",
                host.name
            )));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connect_candidate_pool(candidate: &ReplicaRow) -> Result<PgPool, PgFailoverError> {
    let url = format!(
        "postgres://forgefleet:forgefleet@{}:{POSTGRES_PORT}/forgefleet",
        candidate.primary_ip
    );
    tokio::time::timeout(
        Duration::from_secs(REMOTE_COMMAND_TIMEOUT_SECS),
        PgPoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url),
    )
    .await
    .map_err(|_| {
        PgFailoverError::Promote(format!(
            "direct PostgreSQL connection timed out on {}",
            candidate.name
        ))
    })?
    .map_err(PgFailoverError::Sqlx)
}

async fn commit_authority_cas(
    pool: &PgPool,
    old_primary: &PrimaryRow,
    candidate: &SelectedCandidate,
    estimated_rpo_seconds: i64,
) -> Result<(), PgFailoverError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('forgefleet:postgres-primary-authority'))")
        .execute(&mut *tx)
        .await?;

    let old = sqlx::query(
        "UPDATE database_replicas
            SET role='standby', status='stopped'
          WHERE computer_id=$1 AND database_kind='postgres'
            AND role='primary' AND status='running'",
    )
    .bind(old_primary.computer_id)
    .execute(&mut *tx)
    .await?;
    if old.rows_affected() != 1 {
        return Err(PgFailoverError::Authority(format!(
            "old-primary CAS affected {} rows, expected 1",
            old.rows_affected()
        )));
    }

    let now = Utc::now();
    let promoted = sqlx::query(
        "UPDATE database_replicas
            SET role='primary', status='running', promoted_at=$2,
                lag_bytes=0, last_sync_at=$2,
                notes=concat_ws(';', NULLIF(notes,''), $3)
          WHERE computer_id=$1 AND database_kind='postgres'
            AND role='replica' AND status='running'",
    )
    .bind(candidate.row.computer_id)
    .bind(now)
    .bind(format!(
        "failover_from={};async_rpo_bytes={};async_rpo_seconds={estimated_rpo_seconds}",
        old_primary.name,
        candidate.row.lag_bytes.unwrap_or(i64::MAX)
    ))
    .execute(&mut *tx)
    .await?;
    if promoted.rows_affected() != 1 {
        return Err(PgFailoverError::Authority(format!(
            "candidate CAS affected {} rows, expected 1",
            promoted.rows_affected()
        )));
    }

    sqlx::query(
        "UPDATE database_replicas
            SET status='needs_repoint'
          WHERE database_kind='postgres' AND role='replica'
            AND computer_id<>$1",
    )
    .bind(candidate.row.computer_id)
    .execute(&mut *tx)
    .await?;

    let new_url = format!(
        "postgres://forgefleet:forgefleet@{}:{POSTGRES_PORT}/forgefleet",
        candidate.row.primary_ip
    );
    sqlx::query(
        "INSERT INTO dsn_of_record
            (singleton_key,dsn,primary_member,previous_dsn,updated_at,updated_by)
         VALUES ('current',$1,$2,NULL,NOW(),'pg_failover')
         ON CONFLICT (singleton_key) DO UPDATE SET
            previous_dsn=dsn_of_record.dsn,
            dsn=EXCLUDED.dsn,
            primary_member=EXCLUDED.primary_member,
            updated_at=NOW(),
            updated_by=EXCLUDED.updated_by",
    )
    .bind(&new_url)
    .bind(&candidate.row.name)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO fleet_secrets (key,value,description,updated_at,updated_by)
         VALUES
            ('db_dsn_of_record',$1,'current PostgreSQL primary DSN',NOW(),'pg_failover'),
            ('postgres_primary_url',$1,'legacy PostgreSQL primary DSN mirror',NOW(),'pg_failover')
         ON CONFLICT (key) DO UPDATE SET
            value=EXCLUDED.value,
            description=EXCLUDED.description,
            expires_at=NULL,
            previous_value=NULL,
            updated_at=NOW(),
            updated_by=EXCLUDED.updated_by",
    )
    .bind(new_url)
    .execute(&mut *tx)
    .await?;

    let primary_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT computer_id FROM database_replicas
          WHERE database_kind='postgres' AND role='primary'
          ORDER BY computer_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    if primary_ids != [candidate.row.computer_id] {
        return Err(PgFailoverError::Authority(format!(
            "post-CAS primary set is not exactly candidate {}",
            candidate.row.name
        )));
    }
    tx.commit().await?;
    Ok(())
}

fn is_disabled() -> bool {
    std::env::var(DISABLE_ENV)
        .map(|value| matches!(value.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

async fn probe_tcp(host: &str, port: u16, timeout_secs: u64) -> bool {
    let address = format!("{host}:{port}");
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        tokio::net::TcpStream::connect(&address),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            debug!(%address, %error, "pg_failover: TCP connect failed");
            false
        }
        Err(_) => {
            debug!(%address, timeout_secs, "pg_failover: TCP connect timed out");
            false
        }
    }
}

async fn fence_old_primary(ssh_user: &str, host: &str) -> bool {
    let target = format!("{ssh_user}@{host}");
    let output = tokio::time::timeout(
        Duration::from_secs(REMOTE_COMMAND_TIMEOUT_SECS),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=5",
                &target,
                FENCE_PRIMARY_COMMAND,
            ])
            .stdin(Stdio::null())
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => {
            info!(%target, "pg_failover: old primary fenced");
            true
        }
        Ok(Ok(output)) => {
            warn!(
                %target,
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "pg_failover: fencing failed"
            );
            false
        }
        Ok(Err(error)) => {
            warn!(%target, %error, "pg_failover: fencing SSH failed");
            false
        }
        Err(_) => {
            warn!(%target, "pg_failover: fencing timed out");
            false
        }
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
            std::env::set_var(DISABLE_ENV, "no");
        }
        assert!(!is_disabled());
        unsafe {
            std::env::remove_var(DISABLE_ENV);
        }
    }

    #[test]
    fn parses_postgres_lsn_and_rejects_invalid_values() {
        assert_eq!(parse_pg_lsn("1A0/E100000"), Some(0x1a000000000 + 0xe100000));
        assert_eq!(parse_pg_lsn("0/0"), Some(0));
        assert_eq!(parse_pg_lsn("garbage"), None);
        assert_eq!(parse_pg_lsn("100000000/0"), None);
    }

    #[test]
    fn lsn_lag_is_fail_closed_for_replica_ahead_or_overflow() {
        assert_eq!(lsn_lag_bytes(10_000, 9_000), Some(1_000));
        assert_eq!(lsn_lag_bytes(9_000, 10_000), None);
        assert_eq!(lsn_lag_bytes(u64::MAX, 0), None);
    }

    #[test]
    fn parses_complete_replica_probe() {
        let probe = parse_replica_probe(
            r#"{"in_recovery":true,"read_only":true,"streaming":true,"replay_lsn":"1A0/E100000","replay_age_seconds":3,"receiver_age_seconds":1}"#,
        )
        .expect("probe");
        assert!(probe.in_recovery);
        assert!(probe.read_only);
        assert!(probe.streaming);
        assert_eq!(probe.replay_lsn, parse_pg_lsn("1A0/E100000"));
        assert_eq!(probe.replay_age_seconds, Some(3));
    }

    #[test]
    fn incomplete_probe_is_not_healthy() {
        let probe = ReplicaProbe {
            in_recovery: true,
            read_only: true,
            streaming: true,
            replay_lsn: Some(1),
            replay_lsn_text: Some("0/1".into()),
            replay_age_seconds: None,
            receiver_age_seconds: Some(1),
        };
        assert!(!replica_probe_healthy(&probe, Some(0)));
        assert!(!replica_probe_healthy(&probe, None));
    }

    #[test]
    fn cache_accepts_only_non_future_bounded_snapshot() {
        let now = Utc::now();
        assert!(topology_cache_is_fresh(
            now,
            now - chrono::Duration::seconds(MAX_TOPOLOGY_CACHE_AGE_SECS)
        ));
        assert!(!topology_cache_is_fresh(
            now,
            now - chrono::Duration::seconds(MAX_TOPOLOGY_CACHE_AGE_SECS + 1)
        ));
        assert!(!topology_cache_is_fresh(
            now,
            now + chrono::Duration::seconds(1)
        ));
    }

    #[test]
    fn remote_commands_contain_no_credentials() {
        for command in [
            replica_probe_remote_command(),
            promoted_primary_probe_remote_command(),
            promote_remote_command(),
        ] {
            let lower = command.to_lowercase();
            assert!(!lower.contains("password"));
            assert!(!lower.contains("postgres://"));
        }
    }

    #[test]
    fn fencing_covers_original_and_promoted_container_and_verifies_port() {
        assert!(FENCE_PRIMARY_COMMAND.contains("forgefleet-postgres"));
        assert!(FENCE_PRIMARY_COMMAND.contains(REPLICA_CONTAINER));
        assert!(FENCE_PRIMARY_COMMAND.contains("55432"));
        assert!(FENCE_PRIMARY_COMMAND.contains("command -v ss"));
        assert!(FENCE_PRIMARY_COMMAND.contains("ss -H -ltn"));
    }

    #[test]
    fn promotion_outcome_surfaces_async_rpo() {
        assert_eq!(
            FailoverOutcome::Promoted {
                target: "duncan".into(),
                estimated_rpo_bytes: 16_408,
                estimated_rpo_seconds: 2,
            },
            FailoverOutcome::Promoted {
                target: "duncan".into(),
                estimated_rpo_bytes: 16_408,
                estimated_rpo_seconds: 2,
            }
        );
    }
}
