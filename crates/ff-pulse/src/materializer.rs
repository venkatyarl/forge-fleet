//! Pulse v2 materializer — turns Redis beats into durable Postgres rows.
//!
//! The materializer runs only on the elected leader. It subscribes to the
//! `pulse:events` Redis pub/sub channel and, on each beat, performs
//! DELTA writes to Postgres: only persistent fields that have changed are
//! written, and ephemeral fields (queue depths, tokens/sec, live CPU%,
//! per-container CPU, etc.) are deliberately NOT written — those remain
//! in Tier-1 Redis only.
//!
//! To avoid redundant DB churn when nothing has actually changed, the
//! materializer caches the last-persisted snapshot of the persistent
//! subset of each beat in Redis under `pulse:persisted:{name}` (1h TTL).
//! If the new beat's persistent subset matches the snapshot exactly, the
//! only UPDATE issued is `computers.last_seen_at = NOW()`.

use std::collections::HashSet;

use futures::StreamExt;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::beat_v2::{AvailableModel, DockerContainer, InstalledSoftware, LlmServer, PulseBeatV2};

/// Channel name this materializer subscribes to.
const PULSE_EVENTS_CHANNEL: &str = "pulse:events";

/// Redis key prefix where last-persisted snapshots live.
const PERSISTED_SNAPSHOT_PREFIX: &str = "pulse:persisted:";

/// TTL for the persisted snapshot in Redis (1 hour).
const PERSISTED_SNAPSHOT_TTL_SECS: u64 = 3600;

/// Q4: the delta-path computers-row write. One atomic statement that
/// refreshes `last_seen_at` and rewrites every persistent field from the
/// beat, so a stale Q1 read can never decide to skip fields another writer
/// changed underneath us. Idempotent: every assignment is a pure function
/// of the bind values, so replaying the same beat converges to the same row.
const UPSERT_COMPUTER_ROW_SQL: &str = "UPDATE computers SET \
    last_seen_at = NOW(), \
    primary_ip = $9, \
    all_ips = $2::jsonb, \
    cpu_cores = $3, \
    total_ram_gb = $4, \
    total_disk_gb = $5, \
    gpu_kind = $6, \
    gpu_count = $7, \
    gpu_total_vram_gb = $8, \
    has_gpu = ($7 > 0) \
 WHERE id = $1";

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MaterializerError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown computer name: {0}")]
    UnknownComputer(String),
}

// -----------------------------------------------------------------------------
// ProcessReport — what changed on a single process_beat call
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ProcessReport {
    pub computer_id: Option<Uuid>,
    pub wrote_computer_row: bool,
    pub software_upserts: usize,
    pub model_presence_upserts: usize,
    pub deployment_upserts: usize,
    pub docker_container_upserts: usize,
    pub ips_updated: bool,
    /// (old_status, new_status) if a status transition occurred.
    pub status_transition: Option<(String, String)>,
    /// How many `fleet_bug_reports` rows were inserted from this beat.
    pub bug_reports_inserted: usize,
}

// -----------------------------------------------------------------------------
// PersistedSnapshot — persistent-only subset cached in Redis
// -----------------------------------------------------------------------------

/// Subset of PulseBeatV2 that is persisted to Postgres. Any field not in
/// here is considered ephemeral and lives only in Redis.
///
/// This is serialized as JSON and stored in Redis under
/// `pulse:persisted:{computer_name}` so the next beat can skip all writes
/// if it matches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedSnapshot {
    pub computer_name: String,
    pub status: String,
    pub all_ips_json: String,
    pub hardware: PersistedHardware,
    pub capabilities: PersistedCapabilities,
    pub installed_software: Vec<PersistedSoftware>,
    pub llm_servers: Vec<PersistedDeployment>,
    pub available_models: Vec<PersistedModel>,
    pub docker_containers: Vec<PersistedContainer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedHardware {
    pub cpu_cores: i32,
    pub ram_gb: i32,
    pub disk_gb: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedCapabilities {
    pub gpu_kind: String,
    pub gpu_count: i32,
    pub gpu_total_vram_gb: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedSoftware {
    pub id: String,
    pub version: String,
    pub install_source: Option<String>,
    pub install_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedDeployment {
    pub deployment_id: Uuid,
    pub model_id: String,
    pub runtime: String,
    pub endpoint: String,
    pub status: String,
    pub cluster_id: Option<String>,
    pub cluster_role: String,
    pub cluster_peers: Vec<String>,
    pub tensor_parallel_size: i32,
    pub pipeline_parallel_size: i32,
    pub pid: Option<i32>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ram_allocated_gb: f64,
    pub vram_allocated_gb: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedModel {
    pub id: String,
    pub size_gb: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistedContainer {
    pub project_name: Option<String>,
    pub compose_file: Option<String>,
    pub container_name: String,
    pub container_id: String,
    pub image: String,
    pub ports: Vec<String>,
    pub status: String,
    pub health: Option<String>,
}

impl PersistedSnapshot {
    /// Build the persistent-only snapshot from a full beat.
    pub(crate) fn from_beat(beat: &PulseBeatV2) -> Self {
        let all_ips_json =
            serde_json::to_string(&beat.network.all_ips).unwrap_or_else(|_| "[]".to_string());

        let hardware = PersistedHardware {
            cpu_cores: beat.hardware.cpu_cores,
            ram_gb: beat.hardware.ram_gb,
            disk_gb: beat.hardware.disk_gb,
        };

        let capabilities = PersistedCapabilities {
            gpu_kind: beat.capabilities.gpu_kind.clone(),
            gpu_count: beat.capabilities.gpu_count,
            gpu_total_vram_gb: beat.capabilities.gpu_total_vram_gb,
        };

        let installed_software = beat
            .installed_software
            .iter()
            .map(|s| PersistedSoftware {
                id: s.id.clone(),
                version: s.version.clone(),
                install_source: s.install_source.clone(),
                install_path: s.install_path.clone(),
            })
            .collect();

        let llm_servers = beat
            .llm_servers
            .iter()
            .map(|s| PersistedDeployment {
                deployment_id: s.deployment_id,
                model_id: s.model.id.clone(),
                runtime: s.runtime.clone(),
                endpoint: s.endpoint.clone(),
                status: s.status.clone(),
                cluster_id: s.cluster.cluster_id.clone(),
                cluster_role: s.cluster.role.clone(),
                cluster_peers: s.cluster.peers.clone(),
                tensor_parallel_size: s.cluster.tensor_parallel_size,
                pipeline_parallel_size: s.cluster.pipeline_parallel_size,
                pid: s.pid,
                started_at: s.started_at,
                ram_allocated_gb: s.memory_used.total_gb,
                vram_allocated_gb: s.gpu_memory_used_gb,
            })
            .collect();

        let available_models = beat
            .available_models
            .iter()
            .map(|m| PersistedModel {
                id: m.id.clone(),
                size_gb: m.size_gb,
            })
            .collect();

        let mut docker_containers = Vec::new();
        for proj in &beat.docker.projects {
            for c in &proj.containers {
                docker_containers.push(PersistedContainer {
                    project_name: Some(proj.name.clone()),
                    compose_file: proj.compose_file.clone(),
                    container_name: c.name.clone(),
                    container_id: c.container_id.clone(),
                    image: c.image.clone(),
                    ports: c.ports.clone(),
                    status: c.status.clone(),
                    health: c.health.clone(),
                });
            }
        }

        Self {
            computer_name: beat.computer_name.clone(),
            status: if beat.going_offline {
                "offline".to_string()
            } else {
                "online".to_string()
            },
            all_ips_json,
            hardware,
            capabilities,
            installed_software,
            llm_servers,
            available_models,
            docker_containers,
        }
    }
}

// -----------------------------------------------------------------------------
// Materializer
// -----------------------------------------------------------------------------

pub struct Materializer {
    pg: sqlx::PgPool,
    redis: redis::Client,
}

impl Materializer {
    pub fn new(pg: sqlx::PgPool, redis: redis::Client) -> Self {
        Self { pg, redis }
    }

    /// Start materializer loop. Subscribes to `pulse:events`. Returns a
    /// JoinHandle that exits when `shutdown` is signalled or the Redis
    /// connection dies irrecoverably.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    info!("materializer: shutdown requested before start");
                    return;
                }

                match self.run_subscribe_loop(&mut shutdown).await {
                    Ok(()) => {
                        info!("materializer: subscribe loop exited cleanly");
                        return;
                    }
                    Err(e) => {
                        error!("materializer: subscribe loop error: {e}; restarting in 5s");
                        tokio::select! {
                            _ = shutdown.changed() => return,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        })
    }

    async fn run_subscribe_loop(
        &self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), MaterializerError> {
        // One-time schema guard: ensure the metadata column exists before we
        // start processing beats. Previously this ran inside upsert_software
        // (hot path), generating ~10 NOTICEs/sec in Postgres logs.
        let _ = sqlx::query(
            "ALTER TABLE computer_software \
                ADD COLUMN IF NOT EXISTS metadata JSONB NOT NULL DEFAULT '{}'::jsonb",
        )
        .execute(&self.pg)
        .await;

        let mut pubsub = self.redis.get_async_pubsub().await?;
        pubsub.subscribe(PULSE_EVENTS_CHANNEL).await?;
        info!(
            "materializer: subscribed to Redis channel {}",
            PULSE_EVENTS_CHANNEL
        );

        let mut stream = pubsub.on_message();

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        info!("materializer: shutdown received, exiting");
                        return Ok(());
                    }
                }
                maybe_msg = stream.next() => {
                    let Some(msg) = maybe_msg else {
                        warn!("materializer: pubsub stream ended");
                        return Ok(());
                    };
                    let payload: String = match msg.get_payload() {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("materializer: non-string payload: {e}");
                            continue;
                        }
                    };
                    // ── HMAC verification ─────────────────────────────
                    // Reject beats that fail HMAC if a pulse_beat_hmac_key
                    // is configured; accept unsigned beats silently when
                    // no key is configured (rollout compatibility).
                    let name_preview = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v.get("computer_name")
                            .and_then(|n| n.as_str())
                            .map(str::to_string))
                        .unwrap_or_else(|| "unknown".into());
                    let hmac_key = crate::pulse_hmac::KeyCache::global().get().await;
                    let outcome = crate::pulse_hmac::verify_json(hmac_key.as_deref(), &payload);
                    if !crate::pulse_hmac::log_verify(&name_preview, outcome) {
                        continue;
                    }
                    let beat: PulseBeatV2 = match serde_json::from_str(&payload) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!("materializer: failed to parse beat: {e}");
                            continue;
                        }
                    };
                    match self.process_beat(&beat).await {
                        Ok(report) => {
                            debug!(
                                computer = %beat.computer_name,
                                wrote_computer_row = report.wrote_computer_row,
                                software_upserts = report.software_upserts,
                                deployment_upserts = report.deployment_upserts,
                                container_upserts = report.docker_container_upserts,
                                bug_reports_inserted = report.bug_reports_inserted,
                                "materializer: beat processed"
                            );
                            // Mirror member status transitions to NATS (best-effort).
                            if let Some((prev, new)) = &report.status_transition {
                                crate::nats::publish_member_status_transition(
                                    &beat.computer_name,
                                    prev,
                                    new,
                                )
                                .await;
                            }
                        }
                        Err(e) => {
                            error!(
                                computer = %beat.computer_name,
                                "materializer: error processing beat: {e}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Process a single beat — the core logic, exposed for testing.
    ///
    /// Issues (at most) the following queries, in order:
    ///   Q1  SELECT_COMPUTER_BY_NAME
    ///   Q2  GET_PERSISTED_SNAPSHOT (redis)
    ///   Q3  UPDATE_COMPUTER_LAST_SEEN_ONLY              (fast-path, snapshot match)
    ///   Q4  UPSERT_COMPUTER_PERSISTENT_FIELDS           (delta-path, one atomic stmt)
    ///   Q5  UPDATE_COMPUTER_STATUS_TRANSITION           (on status change)
    ///   Q6  INSERT_DOWNTIME_EVENT                       (going_offline)
    ///   Q7  CLOSE_DOWNTIME_EVENT                        (return online)
    ///   Q8  UPSERT_COMPUTER_SOFTWARE                    (per software row)
    ///   Q9  SELECT_SOFTWARE_LATEST_VERSION              (per software row)
    ///   Q10 UPSERT_MODEL_PRESENCE                       (per available model)
    ///   Q11 MARK_ABSENT_MODELS_NOT_PRESENT              (batch)
    ///   Q12 UPSERT_MODEL_DEPLOYMENT                     (per llm server)
    ///   Q13 UPSERT_DOCKER_CONTAINER                     (per container)
    ///   Q14 MARK_MISSING_CONTAINERS_STOPPED             (batch)
    ///   Q15 SET_PERSISTED_SNAPSHOT (redis)
    pub async fn process_beat(
        &self,
        beat: &PulseBeatV2,
    ) -> Result<ProcessReport, MaterializerError> {
        let mut report = ProcessReport::default();

        // Q1: look up computer row
        let row = sqlx::query(
            "SELECT id, status, primary_ip, all_ips::text AS all_ips_text, cpu_cores, \
             total_ram_gb, total_disk_gb, gpu_kind, gpu_count, gpu_total_vram_gb \
             FROM computers WHERE name = $1",
        )
        .bind(&beat.computer_name)
        .fetch_optional(&self.pg)
        .await?;

        let row =
            row.ok_or_else(|| MaterializerError::UnknownComputer(beat.computer_name.clone()))?;

        let computer_id: Uuid = row.try_get("id")?;
        let prev_status: String = row.try_get("status")?;
        let prev_primary_ip: Option<String> = row.try_get("primary_ip").ok();
        let prev_all_ips_text: Option<String> = row.try_get("all_ips_text").ok();
        let prev_cpu_cores: Option<i32> = row.try_get("cpu_cores").ok();
        let prev_ram_gb: Option<i32> = row.try_get("total_ram_gb").ok();
        let prev_disk_gb: Option<i32> = row.try_get("total_disk_gb").ok();
        let prev_gpu_kind: Option<String> = row.try_get("gpu_kind").ok();
        let prev_gpu_count: Option<i32> = row.try_get("gpu_count").ok();
        let prev_gpu_total_vram_gb: Option<f64> = row.try_get("gpu_total_vram_gb").ok();

        if computer_row_has_empty_node_attributes(prev_primary_ip.as_deref(), prev_ram_gb) {
            error!(
                computer = %beat.computer_name,
                primary_ip = ?prev_primary_ip,
                total_ram_gb = ?prev_ram_gb,
                "materializer: computers row has empty primary_ip or RAM"
            );
        }

        report.computer_id = Some(computer_id);

        // V43+: persist encountered bugs before the fast-path exit so panics
        // are recorded even when the rest of the beat is unchanged.
        match self.insert_bug_reports(beat, computer_id).await {
            Ok(n) => report.bug_reports_inserted = n,
            Err(e) => {
                tracing::warn!(
                    computer = %beat.computer_name,
                    error = %e,
                    "materializer: bug report insert failed"
                );
            }
        }

        // Build the persistent snapshot for this beat.
        let new_snapshot = PersistedSnapshot::from_beat(beat);

        // Q2: compare against last-persisted snapshot in Redis.
        let redis_key = format!("{PERSISTED_SNAPSHOT_PREFIX}{}", beat.computer_name);
        let mut redis_conn = self.redis.get_multiplexed_async_connection().await?;
        let prior_snapshot_json: Option<String> = redis_conn.get(&redis_key).await.ok().flatten();
        let prior_snapshot: Option<PersistedSnapshot> = prior_snapshot_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());

        // Determine whether this beat flips the row status (online/offline).
        let transition_plan = plan_status_transition(&prev_status, beat.going_offline);
        let status_changed = transition_plan.is_some();

        if let Some(plan) = &transition_plan {
            report.status_transition = Some((prev_status.clone(), plan.new_status.to_string()));
        }

        // V89: build_sha auto-refresh — must run BEFORE the fast-path exit
        // below, otherwise it's skipped on every beat after the snapshot
        // first matches (which is most of them, since SHA only changes on
        // redeploy). The WHERE `installed_version <> $1` guard makes the
        // UPDATE a true no-op once the row is correct, so running on every
        // beat is cheap.
        if let Some(sha) = beat.build_sha.as_deref() {
            let _ = sqlx::query(
                "UPDATE computer_software SET \
                    installed_version = $1, \
                    last_checked_at = NOW() \
                 WHERE computer_id = $2 \
                   AND software_id IN ('ff_git', 'forgefleetd_git') \
                   AND COALESCE(installed_version, '') <> $1",
            )
            .bind(sha)
            .bind(computer_id)
            .execute(&self.pg)
            .await;
        }

        // source_tree_path self-heal — must also run BEFORE the fast-path
        // exit (it changes only on redeploy, so it's absent from the snapshot
        // and would otherwise never reconcile). The node reports its own tree
        // location as ground truth; the leader's auto-upgrade `cd`s into this
        // column, and a NULL value makes the leader self-upgrade silently skip
        // (surfaced 2026-06-08: only Taylor had it set, so leadership moving
        // to any other node would break self-upgrade). Same idiom as the
        // primary_ip heal below — guarded no-op once the row is correct.
        if let Some(stp) = beat.source_tree_path.as_deref().filter(|s| !s.is_empty()) {
            let _ = sqlx::query(
                "UPDATE computers SET source_tree_path = $1 \
                 WHERE id = $2 AND COALESCE(source_tree_path, '') <> $1",
            )
            .bind(stp)
            .bind(computer_id)
            .execute(&self.pg)
            .await;
        }

        // Subsystem liveness is intentionally outside PersistedSnapshot: it
        // changes every dispatch tick and must be refreshed on the fast path.
        // A legacy beat has no value, so leave the existing timestamp alone
        // during rolling upgrades rather than erasing a newer observation.
        if let Some(dispatch_tick_at) = beat.dispatch_tick_at {
            sqlx::query("UPDATE computers SET dispatch_tick_at = $1 WHERE id = $2")
                .bind(dispatch_tick_at)
                .bind(computer_id)
                .execute(&self.pg)
                .await?;
        }

        // The Redis snapshot is only a write-churn cache, not the source of
        // truth. Another writer may have changed (or partially cleared) the
        // Postgres row since this snapshot was stored, so the fast path is
        // safe only while the row still matches the beat as well.
        let new_ips_json = &new_snapshot.all_ips_json;
        let ips_differ = prev_all_ips_text
            .as_deref()
            .map(|s| normalize_json(s) != normalize_json(new_ips_json))
            .unwrap_or(true);
        let hw_differ = prev_cpu_cores != Some(beat.hardware.cpu_cores)
            || prev_ram_gb != Some(beat.hardware.ram_gb)
            || prev_disk_gb != Some(beat.hardware.disk_gb);
        let cap_differ = prev_gpu_kind.as_deref() != Some(beat.capabilities.gpu_kind.as_str())
            || prev_gpu_count != Some(beat.capabilities.gpu_count)
            || prev_gpu_total_vram_gb != beat.capabilities.gpu_total_vram_gb;
        let primary_ip_differ =
            prev_primary_ip.as_deref() != Some(beat.network.primary_ip.as_str());
        let persistent_row_differ =
            persistent_fields_changed(ips_differ, hw_differ, cap_differ, primary_ip_differ);

        // Fast path: if both cached and durable state match exactly AND no
        // status transition, only update last_seen_at.
        let snapshots_match = prior_snapshot
            .as_ref()
            .map(|ps| ps == &new_snapshot)
            .unwrap_or(false);

        if can_use_last_seen_fast_path(snapshots_match, status_changed, persistent_row_differ) {
            // Q3: UPDATE_COMPUTER_LAST_SEEN_ONLY
            sqlx::query("UPDATE computers SET last_seen_at = NOW() WHERE id = $1")
                .bind(computer_id)
                .execute(&self.pg)
                .await?;

            // Refresh the TTL on the snapshot so it doesn't expire mid-stream.
            let _: Result<(), _> = redis_conn
                .set_ex(
                    &redis_key,
                    serde_json::to_string(&new_snapshot)?,
                    PERSISTED_SNAPSHOT_TTL_SECS,
                )
                .await;

            return Ok(report);
        }

        // Delta path.

        // IPs comparison.
        if ips_differ {
            report.ips_updated = true;
        }

        // primary_ip comparison. Without this the column would be frozen to
        // whichever interface the node had at first enrollment — surfaced
        // 2026-04-28 on aura, where DB primary_ip was a dead wifi address
        // (192.168.5.109) long after the laptop switched to ethernet
        // (192.168.5.110). `ff fleet versions --live` (and any other
        // primary_ip-using ssh path) was effectively unreachable.
        // Q4: UPSERT_COMPUTER_PERSISTENT_FIELDS — single atomic statement.
        //
        // This used to be a read-compare-branch: the Q1 snapshot picked
        // between a full persistent-field UPDATE and a last_seen_at-only
        // touch. That decision raced concurrent writers (overlapping leader
        // during failover, operator UPDATE): a "nothing changed" verdict
        // computed from the stale Q1 read skipped the write and left the
        // beat's values unapplied until the next differing beat. The row is
        // rewritten on every delta-path beat anyway (last_seen_at), so one
        // unconditional statement carrying every persistent field costs no
        // extra tuple write, cannot interleave with other writers, and is
        // idempotent — reapplying the same beat yields the same row. The
        // row itself is created at enrollment (`ssh_user`/`os_family` are
        // NOT NULL and absent from beats), which is why this is an UPDATE
        // keyed on id rather than an INSERT .. ON CONFLICT: an unknown
        // computer must stay an UnknownComputer error, not auto-enroll.
        //
        // Guard: a skeleton/degenerate beat (empty primary_ip, or
        // ram_gb <= 0 — e.g. a daemon publishing before hardware probing
        // finished) must not be allowed to clobber a previously-good row
        // with empty values. Reject the whole persistent-field write in
        // that case and fall back to a last_seen_at-only touch; the next
        // beat with real hardware data will apply on the normal delta path.
        if computer_row_has_empty_node_attributes(
            Some(beat.network.primary_ip.as_str()),
            Some(beat.hardware.ram_gb),
        ) {
            warn!(
                computer = %beat.computer_name,
                primary_ip = %beat.network.primary_ip,
                ram_gb = beat.hardware.ram_gb,
                "materializer: beat has empty primary_ip or non-positive ram_gb; \
                 rejecting computers-row upsert to avoid corrupting persisted values"
            );
            sqlx::query("UPDATE computers SET last_seen_at = NOW() WHERE id = $1")
                .bind(computer_id)
                .execute(&self.pg)
                .await?;
        } else {
            // Track transient empty writes: primary_ip/ram are already gated
            // above, but a partially-probed beat can still carry an empty
            // all_ips / zero cpu_cores / zero disk / empty gpu_kind that this
            // (non-rejected) write persists onto the row. Log the exact column
            // set so the "computers row briefly went empty" drift is traceable
            // to the beat that caused it.
            let empty_fields = empty_persistent_beat_fields(beat, new_ips_json);
            if !empty_fields.is_empty() {
                warn!(
                    computer = %beat.computer_name,
                    empty_fields = ?empty_fields,
                    primary_ip = %beat.network.primary_ip,
                    all_ips = %new_ips_json,
                    cpu_cores = beat.hardware.cpu_cores,
                    ram_gb = beat.hardware.ram_gb,
                    disk_gb = beat.hardware.disk_gb,
                    gpu_kind = %beat.capabilities.gpu_kind,
                    "materializer: updating computers row with empty persistent value(s)"
                );
            }
            sqlx::query(UPSERT_COMPUTER_ROW_SQL)
                .bind(computer_id)
                .bind(new_ips_json)
                .bind(beat.hardware.cpu_cores)
                .bind(beat.hardware.ram_gb)
                .bind(beat.hardware.disk_gb)
                .bind(&beat.capabilities.gpu_kind)
                .bind(beat.capabilities.gpu_count)
                .bind(beat.capabilities.gpu_total_vram_gb)
                .bind(&beat.network.primary_ip)
                .execute(&self.pg)
                .await?;
            report.wrote_computer_row = persistent_row_differ;
        }

        // Always keep fleet_workers.ip in sync with the heartbeat's
        // primary_ip — this is the worker-role registry (V83 rename from
        // fleet_nodes) that ff fleet computers / deploy scripts read from.
        // Without this, the column drifted (operationally bit us: 9/15
        // computers wrong, undetected for weeks — see [[db-ip-corruption-20260512]]).
        //
        // Cheap no-op UPDATE: the WHERE `ip <> $1` filter means PG only
        // writes a tuple if the IP actually changed. The earlier `if
        // primary_ip_differ` gate was wrong — it only fired when
        // `computers.primary_ip` had drifted, which is a different signal
        // from `fleet_workers.ip` drift. Always-run with row-level guard
        // is correct.
        // Capture the result: this is a self-heal path for a column that drifted
        // undetected for weeks (9/15 computers wrong). Silently swallowing a
        // failure here would re-hide exactly that class of bug, so log it.
        // Also self-heal ram_gb + cpu_cores, which drifted exactly like ip did:
        // they were never updated after enrollment, so 13/15 workers were stuck
        // at the 8GB/4-core enrollment placeholder while `computers` carried the
        // real hardware (marcus is 31GB/12c). Wrong specs make the autoscaler /
        // placement skip these nodes — why so many sat with no model. Guard each
        // with `$N > 0` (and ip with `$1 <> ''`) so a degenerate beat can't zero
        // a good value.
        match sqlx::query(
            "UPDATE fleet_workers SET \
                 ip = CASE WHEN $1 <> '' THEN $1 ELSE ip END, \
                 ram_gb = CASE WHEN $3 > 0 THEN $3 ELSE ram_gb END, \
                 cpu_cores = CASE WHEN $4 > 0 THEN $4 ELSE cpu_cores END, \
                 updated_at = NOW() \
             WHERE name = $2 AND ( \
                 ($1 <> '' AND ip <> $1) \
                 OR ($3 > 0 AND ram_gb IS DISTINCT FROM $3) \
                 OR ($4 > 0 AND cpu_cores IS DISTINCT FROM $4))",
        )
        .bind(&beat.network.primary_ip)
        .bind(&beat.computer_name)
        .bind(beat.hardware.ram_gb)
        .bind(beat.hardware.cpu_cores)
        .execute(&self.pg)
        .await
        {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::info!(
                    computer = %beat.computer_name,
                    ip = %beat.network.primary_ip,
                    ram_gb = beat.hardware.ram_gb,
                    cpu_cores = beat.hardware.cpu_cores,
                    "materializer: reconciled drifted fleet_workers ip/ram_gb/cpu_cores"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                computer = %beat.computer_name,
                error = %e,
                "materializer: fleet_workers ip/ram/cpu self-heal UPDATE failed"
            ),
        }

        // V87: keep computers.os_family / os_distribution / os_version in sync
        // with the beat's pre-classified OsInfo. Daemons detect their own
        // OS (kernel + /etc/os-release) and ship it through `beat.os`, so
        // the auto-upgrade playbook resolver always finds the right key
        // (linux-ubuntu vs linux-dgx vs macos) without manual classification.
        // Empty family means the beat was published by an older daemon
        // that doesn't carry `os` yet — skip in that case.
        if !beat.os.family.is_empty() && beat.os.family != "unknown" {
            let _ = sqlx::query(
                "UPDATE computers SET \
                    os_family = $1, \
                    os_distribution = NULLIF($2, ''), \
                    os_version = NULLIF($3, '') \
                 WHERE id = $4 AND ( \
                    os_family <> $1 \
                    OR COALESCE(os_distribution, '') <> $2 \
                    OR COALESCE(os_version, '') <> $3 \
                 )",
            )
            .bind(&beat.os.family)
            .bind(&beat.os.distribution)
            .bind(&beat.os.version)
            .bind(computer_id)
            .execute(&self.pg)
            .await;
        }

        // V165: resolve the hardware→inference-server decision table
        // (fleet_server_policies, kind='server_policy') against this beat's
        // detected hardware and self-heal fleet_workers.runtime; on a real
        // (re)classification it also seeds the policy's model downloads.
        // Best-effort: a missing table/row must never fail the beat.
        self.apply_server_policy(beat).await;

        // build_sha auto-refresh moved earlier in this function so it
        // runs before the fast-path exit; see V89 note above.

        // Status transition handling (Q5–Q7).
        //
        // Race-safety: `status` was read at Q1, but another writer — an
        // overlapping leader during failover, a duplicate beat for the same
        // computer, an operator UPDATE — may have moved the row since. The
        // old unconditional UPDATE + separate event INSERT let both racers
        // record the same transition (duplicate downtime events, clobbered
        // offline_since, phantom node_online publishes). The flip is now a
        // compare-and-set (`WHERE status = $prev`) and its downtime-event
        // bookkeeping runs in the SAME transaction, gated on the CAS
        // affecting a row: the racer that loses sees rows_affected == 0 and
        // skips both the event write and the publish.
        if let Some(plan) = &transition_plan {
            let mut tx = self.pg.begin().await?;

            // Q5: atomic compare-and-set of the status flip.
            let flipped = sqlx::query(
                "UPDATE computers SET \
                    status = $2, \
                    status_changed_at = NOW(), \
                    offline_since = CASE WHEN $2 = 'offline' \
                        THEN COALESCE(offline_since, NOW()) \
                        ELSE NULL END \
                 WHERE id = $1 AND status = $3",
            )
            .bind(computer_id)
            .bind(plan.new_status)
            .bind(&prev_status)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0;

            if flipped {
                if plan.insert_downtime {
                    // Q6: open a downtime event for the offline transition.
                    sqlx::query(
                        "INSERT INTO computer_downtime_events \
                            (computer_id, offline_at, cause) \
                         VALUES ($1, NOW(), 'graceful_shutdown')",
                    )
                    .bind(computer_id)
                    .execute(&mut *tx)
                    .await?;
                }
                if plan.close_downtime {
                    // Q7: close the most recent open downtime event, if any.
                    sqlx::query(
                        "UPDATE computer_downtime_events \
                         SET online_at = NOW(), \
                             duration_sec = EXTRACT(EPOCH FROM (NOW() - offline_at))::INT, \
                             resolved_by = 'pulse_return' \
                         WHERE computer_id = $1 AND online_at IS NULL",
                    )
                    .bind(computer_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }

            tx.commit().await?;

            if flipped {
                report.wrote_computer_row = true;
                if plan.publish_node_online {
                    // Wake the deferred-task scheduler: any task with
                    // trigger=node_online targeting this computer is
                    // queued waiting on this exact event. Without this
                    // publish, those tasks sit forever even though the
                    // node has been online for hours — only `version_check_pass`
                    // (every 6h, drift-only) was firing the event before.
                    // Publish channel name is mirrored from
                    // `ff_agent::fleet_events::CHANNEL_NODE_ONLINE`; we
                    // can't import that without a circular crate dep.
                    use redis::AsyncCommands;
                    let _: Result<(), _> = redis_conn
                        .publish::<_, _, ()>("fleet:node_online", &beat.computer_name)
                        .await;
                    tracing::info!(
                        computer = %beat.computer_name,
                        prev_status = %prev_status,
                        "materializer: published fleet:node_online for sdown→online transition"
                    );
                }
            } else {
                // Lost the race: another writer already moved the row off
                // `prev_status`. Clear the transition so the NATS mirror in
                // the subscribe loop doesn't announce a phantom flip.
                report.status_transition = None;
            }
        }

        // Software upserts (Q8, Q9).
        //
        // Per-row resilience: a single FK violation (software_id not in
        // software_registry) used to propagate via `?`, aborting the entire
        // beat — and silently zeroing every subsequent upsert. Surfaced
        // 2026-04-29: the collector emits `ff` and `forgefleetd` rows but
        // software_registry only had `ff_git` and `forgefleetd_git`, so the
        // very first iteration FK'd and no software_upsert had run for ~2
        // days. ALL fleet drift was frozen as a result.
        //
        // V64 adds the missing registry rows. This change makes the loop
        // resilient even if the schemas drift again: log + skip unknown
        // software_ids, keep processing the rest of the beat.
        for sw in &beat.installed_software {
            match self.upsert_software(computer_id, sw).await {
                Ok(_) => {
                    report.software_upserts += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        computer = %beat.computer_name,
                        software_id = %sw.id,
                        error = %e,
                        "materializer: skipping software upsert (continuing with rest of beat)"
                    );
                }
            }
        }

        // Model presence upserts (Q10).
        for m in &beat.available_models {
            self.upsert_model_presence(computer_id, m).await?;
            report.model_presence_upserts += 1;
        }

        // Q11: mark models NOT in beat's list as absent (only the ones whose
        // last_seen_at is older than 5 minutes — spec).
        let present_ids: Vec<String> = beat.available_models.iter().map(|m| m.id.clone()).collect();
        sqlx::query(
            "UPDATE computer_models \
             SET present = false \
             WHERE computer_id = $1 \
               AND last_seen_at < NOW() - INTERVAL '5 minutes' \
               AND NOT (model_id = ANY($2))",
        )
        .bind(computer_id)
        .bind(&present_ids)
        .execute(&self.pg)
        .await?;

        // Deployment upserts (Q12). Capture the pre-upsert timestamp so we
        // can find rows for this computer that the beat DIDN'T touch and
        // mark them stopped (Q12b). Without this prune, deployments that
        // vanished — a vllm container torn down, a llama-server crashed
        // out, an endpoint retired — stay `active` in the DB forever; the
        // fleet dashboard then reports phantom capacity.
        let prune_before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT NOW()")
            .fetch_one(&self.pg)
            .await?;
        for s in &beat.llm_servers {
            self.upsert_deployment(computer_id, s).await?;
            report.deployment_upserts += 1;
        }
        // Q12b: mark any deployment row for this computer that wasn't
        // upserted by this beat as `stopped`. Mirrors Q14's prune for
        // containers. `last_status_change` is refreshed by every upsert
        // above, so anything older than `prune_before` is a row the beat
        // did not refer to.
        sqlx::query(
            "UPDATE computer_model_deployments \
             SET status = 'stopped', last_status_change = NOW() \
             WHERE computer_id = $1 \
               AND status <> 'stopped' \
               AND last_status_change < $2",
        )
        .bind(computer_id)
        .bind(prune_before)
        .execute(&self.pg)
        .await?;

        // Docker container upserts (Q13).
        let mut seen_container_names: HashSet<String> = HashSet::new();
        for proj in &beat.docker.projects {
            for c in &proj.containers {
                self.upsert_docker_container(
                    computer_id,
                    Some(&proj.name),
                    proj.compose_file.as_deref(),
                    c,
                )
                .await?;
                report.docker_container_upserts += 1;
                seen_container_names.insert(c.name.clone());
            }
        }

        // Q14: mark missing containers as stopped (only those this computer
        // owns that weren't in the beat).
        let seen: Vec<String> = seen_container_names.into_iter().collect();
        sqlx::query(
            "UPDATE computer_docker_containers \
             SET status = 'stopped', last_status_change = NOW() \
             WHERE computer_id = $1 \
               AND status <> 'stopped' \
               AND NOT (container_name = ANY($2))",
        )
        .bind(computer_id)
        .bind(&seen)
        .execute(&self.pg)
        .await?;

        // V43: upsert fabric pairs from reciprocal cx7-fabric IP claims.
        // Soft-fail (log + continue) — a fabric-upsert error should not
        // abort the whole beat materialization.
        if let Err(e) = crate::fabric_upsert::upsert_fabric_pairs(&self.pg, beat, computer_id).await
        {
            tracing::warn!(computer = %beat.computer_name, error = %e, "fabric_pairs upsert failed");
        }
        if let Err(e) =
            crate::fabric_upsert::upsert_ray_memberships(&self.pg, beat, computer_id).await
        {
            tracing::warn!(computer = %beat.computer_name, error = %e, "llm_clusters upsert failed");
        }

        // Q15: write new snapshot for next-beat delta compare.
        let snapshot_json = serde_json::to_string(&new_snapshot)?;
        let _: Result<(), _> = redis_conn
            .set_ex(&redis_key, snapshot_json, PERSISTED_SNAPSHOT_TTL_SECS)
            .await;

        Ok(report)
    }

    // -------------------------------------------------------------------------
    // Per-row helpers
    // -------------------------------------------------------------------------

    async fn upsert_software(
        &self,
        computer_id: Uuid,
        sw: &InstalledSoftware,
    ) -> Result<(), MaterializerError> {
        // Q9: look up latest_version from registry to compute upgrade status.
        let latest_row = sqlx::query("SELECT latest_version FROM software_registry WHERE id = $1")
            .bind(&sw.id)
            .fetch_optional(&self.pg)
            .await?;

        let latest_version: Option<String> = latest_row.and_then(|r| {
            r.try_get::<Option<String>, _>("latest_version")
                .ok()
                .flatten()
        });

        let new_status: &str = match latest_version.as_deref() {
            Some(lv) if !lv.is_empty() && lv != sw.version => "upgrade_available",
            _ => "ok",
        };

        // Q8: UPSERT.
        //
        // Only bump `status` when installed_version itself changed: we
        // compare the pre-existing row's installed_version to the incoming
        // one in the ON CONFLICT clause using a WHERE on the excluded row.
        //
        // `metadata` is merged into the existing row (jsonb concat) so
        // keys like `git_state` are preserved across beats that don't
        // resend them.
        let meta = sw.metadata.clone().unwrap_or_else(|| serde_json::json!({}));
        sqlx::query(
            "INSERT INTO computer_software \
                (computer_id, software_id, installed_version, install_source, install_path, \
                 last_checked_at, status, metadata) \
             VALUES ($1, $2, $3, $4, $5, NOW(), $6, $7) \
             ON CONFLICT (computer_id, software_id) DO UPDATE SET \
                installed_version = EXCLUDED.installed_version, \
                install_source    = EXCLUDED.install_source, \
                install_path      = EXCLUDED.install_path, \
                last_checked_at   = NOW(), \
                metadata          = computer_software.metadata || EXCLUDED.metadata, \
                status = CASE \
                    WHEN computer_software.installed_version IS DISTINCT FROM EXCLUDED.installed_version \
                        THEN EXCLUDED.status \
                    ELSE computer_software.status \
                END",
        )
        .bind(computer_id)
        .bind(&sw.id)
        .bind(&sw.version)
        .bind(sw.install_source.as_deref())
        .bind(sw.install_path.as_deref())
        .bind(new_status)
        .bind(&meta)
        .execute(&self.pg)
        .await?;

        Ok(())
    }

    async fn upsert_model_presence(
        &self,
        computer_id: Uuid,
        m: &AvailableModel,
    ) -> Result<(), MaterializerError> {
        // Synthesize a path when not provided by the beat (the beat schema
        // doesn't carry a file_path yet).
        let file_path = format!("~/models/{}", m.id);

        // Q10: UPSERT.
        sqlx::query(
            "INSERT INTO computer_models \
                (computer_id, model_id, file_path, size_gb, present, last_seen_at) \
             VALUES ($1, $2, $3, $4, true, NOW()) \
             ON CONFLICT (computer_id, model_id) DO UPDATE SET \
                file_path    = EXCLUDED.file_path, \
                size_gb      = EXCLUDED.size_gb, \
                present      = true, \
                last_seen_at = NOW()",
        )
        .bind(computer_id)
        .bind(&m.id)
        .bind(&file_path)
        .bind(m.size_gb)
        .execute(&self.pg)
        .await?;

        Ok(())
    }

    async fn upsert_deployment(
        &self,
        computer_id: Uuid,
        s: &LlmServer,
    ) -> Result<(), MaterializerError> {
        let cluster_peers_json =
            serde_json::to_string(&s.cluster.peers).unwrap_or_else(|_| "[]".to_string());

        // Self-heal: if the incoming beat reports `runtime=""` or
        // `"unknown"` for this (computer, model), check whether a
        // known-runtime row already exists. If yes, refresh its
        // `last_status_change` + `status` and return early instead of
        // creating a duplicate row. Closes the loop identified in
        // `project_pulse_materializer_vllm_runtime_gap.md`:
        // docker-run vLLM on DGX Sparks reports `runtime: null` because
        // forgefleetd didn't launch the container, and the old upsert
        // path (keyed on runtime) would insert a new "unknown" row every
        // 15 s alongside the authoritative "vllm" row.
        //
        // Logic authored by Qwen3-Coder-30B on marcus; hunk-header math
        // fixed on the supervisor side so `git apply` accepts it.
        if s.runtime.is_empty() || s.runtime == "unknown" {
            let known_row = sqlx::query(
                "SELECT id, runtime FROM computer_model_deployments \
                 WHERE computer_id = $1 AND model_id = $2 \
                   AND runtime IN ('vllm', 'llama.cpp', 'mlx_lm', 'ollama') \
                 LIMIT 1",
            )
            .bind(computer_id)
            .bind(&s.model.id)
            .fetch_optional(&self.pg)
            .await?;

            if let Some(row) = known_row {
                let existing_id: Uuid = row.try_get("id")?;
                let known_runtime: String = row.try_get("runtime")?;
                tracing::debug!(
                    computer_id = %computer_id,
                    model_id = %s.model.id,
                    known_runtime = %known_runtime,
                    "skipped unknown-runtime upsert; existing known-runtime row refreshed"
                );
                sqlx::query(
                    "UPDATE computer_model_deployments \
                        SET last_status_change = NOW(), status = $1 \
                      WHERE id = $2",
                )
                .bind(&s.status)
                .bind(existing_id)
                .execute(&self.pg)
                .await?;
                return Ok(());
            }
        }

        // Q12: UPSERT keyed on (computer_id, model_id, runtime, endpoint).
        // There is no unique constraint on that tuple in V14, so we emulate
        // it: look for a match first, UPDATE if found, else INSERT. The
        // deployment_id on the beat is authoritative when present.
        let existing = sqlx::query(
            "SELECT id FROM computer_model_deployments \
             WHERE computer_id = $1 AND model_id = $2 AND runtime = $3 AND endpoint = $4 \
             LIMIT 1",
        )
        .bind(computer_id)
        .bind(&s.model.id)
        .bind(&s.runtime)
        .bind(&s.endpoint)
        .fetch_optional(&self.pg)
        .await?;

        if let Some(row) = existing {
            let existing_id: Uuid = row.try_get("id")?;
            sqlx::query(
                "UPDATE computer_model_deployments SET \
                    status                 = $2, \
                    cluster_id             = $3, \
                    cluster_role           = $4, \
                    cluster_peers          = $5::jsonb, \
                    tensor_parallel_size   = $6, \
                    pipeline_parallel_size = $7, \
                    pid                    = $8, \
                    started_at             = $9, \
                    ram_allocated_gb       = $10, \
                    vram_allocated_gb      = $11, \
                    context_window         = $12, \
                    parallel_slots         = $13, \
                    openai_compatible      = $14, \
                    last_status_change     = NOW() \
                 WHERE id = $1",
            )
            .bind(existing_id)
            .bind(&s.status)
            .bind(s.cluster.cluster_id.as_deref())
            .bind(&s.cluster.role)
            .bind(&cluster_peers_json)
            .bind(s.cluster.tensor_parallel_size)
            .bind(s.cluster.pipeline_parallel_size)
            .bind(s.pid)
            .bind(s.started_at)
            .bind(s.memory_used.total_gb)
            .bind(s.gpu_memory_used_gb)
            .bind(s.model.context_window)
            .bind(s.model.parallel_slots)
            .bind(s.openai_compatible)
            .execute(&self.pg)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO computer_model_deployments \
                    (id, computer_id, model_id, runtime, endpoint, openai_compatible, \
                     context_window, parallel_slots, pid, status, \
                     cluster_id, cluster_role, cluster_peers, \
                     tensor_parallel_size, pipeline_parallel_size, \
                     ram_allocated_gb, vram_allocated_gb, started_at, last_status_change) \
                 VALUES \
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::jsonb, \
                     $14, $15, $16, $17, $18, NOW())",
            )
            .bind(s.deployment_id)
            .bind(computer_id)
            .bind(&s.model.id)
            .bind(&s.runtime)
            .bind(&s.endpoint)
            .bind(s.openai_compatible)
            .bind(s.model.context_window)
            .bind(s.model.parallel_slots)
            .bind(s.pid)
            .bind(&s.status)
            .bind(s.cluster.cluster_id.as_deref())
            .bind(&s.cluster.role)
            .bind(&cluster_peers_json)
            .bind(s.cluster.tensor_parallel_size)
            .bind(s.cluster.pipeline_parallel_size)
            .bind(s.memory_used.total_gb)
            .bind(s.gpu_memory_used_gb)
            .bind(s.started_at)
            .execute(&self.pg)
            .await?;
        }

        Ok(())
    }

    async fn upsert_docker_container(
        &self,
        computer_id: Uuid,
        project_name: Option<&str>,
        compose_file: Option<&str>,
        c: &DockerContainer,
    ) -> Result<(), MaterializerError> {
        let ports_json = serde_json::to_string(&c.ports).unwrap_or_else(|_| "[]".to_string());

        // Q13: UPSERT by (computer_id, container_name).
        sqlx::query(
            "INSERT INTO computer_docker_containers \
                (computer_id, project_name, compose_file, container_name, container_id, \
                 image, ports, status, health, last_seen_at, last_status_change) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8, $9, NOW(), NOW()) \
             ON CONFLICT (computer_id, container_name) DO UPDATE SET \
                project_name       = EXCLUDED.project_name, \
                compose_file       = EXCLUDED.compose_file, \
                container_id       = EXCLUDED.container_id, \
                image              = EXCLUDED.image, \
                ports              = EXCLUDED.ports, \
                health             = EXCLUDED.health, \
                last_seen_at       = NOW(), \
                status             = EXCLUDED.status, \
                last_status_change = CASE \
                    WHEN computer_docker_containers.status IS DISTINCT FROM EXCLUDED.status \
                        THEN NOW() \
                    ELSE computer_docker_containers.last_status_change \
                END",
        )
        .bind(computer_id)
        .bind(project_name)
        .bind(compose_file)
        .bind(&c.name)
        .bind(&c.container_id)
        .bind(&c.image)
        .bind(&ports_json)
        .bind(&c.status)
        .bind(c.health.as_deref())
        .execute(&self.pg)
        .await?;

        Ok(())
    }

    /// Insert `encountered_bugs` from a beat into `fleet_bug_reports`.
    /// Uses ON CONFLICT DO NOTHING so duplicate signatures from the same
    /// daemon are silently deduped within the unique-constraint window.
    async fn insert_bug_reports(
        &self,
        beat: &PulseBeatV2,
        computer_id: Uuid,
    ) -> Result<usize, MaterializerError> {
        if beat.encountered_bugs.is_empty() {
            return Ok(0);
        }
        let mut count = 0usize;
        for bug in &beat.encountered_bugs {
            let rows_affected = sqlx::query(
                "INSERT INTO fleet_bug_reports \
                    (bug_signature, file_path, line_number, error_class, \
                     stack_excerpt, reporting_computer_id, reported_at, \
                     binary_version, tier) \
                 VALUES ($1, $2, $3, $4, $5, $6, NOW(), $7, $8) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(&bug.signature)
            .bind(&bug.file_path)
            .bind(bug.line_number.map(|n| n as i32))
            .bind(&bug.error_class)
            .bind(&bug.stack_excerpt)
            .bind(computer_id)
            .bind(&bug.binary_version)
            .bind(&bug.tier)
            .execute(&self.pg)
            .await?
            .rows_affected();
            count += rows_affected as usize;
        }
        Ok(count)
    }

    /// V165: resolve the DB-persisted hardware→inference-server decision
    /// table (`fleet_server_policies`, kind='server_policy', keyed on
    /// arch/gpu_kind/has_discrete_vram/ram_tier with 'any' wildcards) and
    /// self-heal `fleet_workers.runtime` to the matched row's runtime.
    ///
    /// When the UPDATE actually flips the value — first beat after
    /// enrollment, or a hardware/policy change — the node was just
    /// (re)classified, and we additionally enqueue the row's
    /// `seed_model_ids` as `ff model download <id>` deferred tasks on that
    /// node (dedup'd against its model library and still-open seed tasks).
    /// Best-effort throughout: any failure logs and leaves the beat alone.
    async fn apply_server_policy(&self, beat: &PulseBeatV2) {
        let gpu_kind = beat.capabilities.gpu_kind.as_str();
        // Skeleton/degenerate beats carry no hardware — don't classify.
        if beat.hardware.ram_gb <= 0 || gpu_kind.is_empty() {
            return;
        }
        let arch = resolve_arch(&beat.os.arch, &beat.os.family, gpu_kind);
        let discrete = if has_discrete_vram(
            gpu_kind,
            &beat.os.family,
            beat.capabilities.gpu_total_vram_gb,
        ) {
            "yes"
        } else {
            "no"
        };
        let tier = ram_tier(beat.hardware.ram_gb);

        // Most-specific matching row wins: each concrete key column match
        // scores 1, wildcards 0; `id` breaks ties deterministically.
        let policy = match sqlx::query(
            "SELECT runtime, primary_server, seed_model_ids::text AS seed_ids \
             FROM fleet_server_policies \
             WHERE kind = 'server_policy' \
               AND arch IN ($1, 'any') \
               AND gpu_kind IN ($2, 'any') \
               AND has_discrete_vram IN ($3, 'any') \
               AND ram_tier IN ($4, 'any') \
             ORDER BY ((arch = $1)::int + (gpu_kind = $2)::int \
                     + (has_discrete_vram = $3)::int + (ram_tier = $4)::int) DESC, \
                      id \
             LIMIT 1",
        )
        .bind(&arch)
        .bind(gpu_kind)
        .bind(discrete)
        .bind(tier)
        .fetch_optional(&self.pg)
        .await
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                debug!(
                    computer = %beat.computer_name,
                    arch, gpu_kind, discrete, tier,
                    "server policy: no matching fleet_server_policies row"
                );
                return;
            }
            Err(e) => {
                warn!(
                    computer = %beat.computer_name,
                    error = %e,
                    "server policy: fleet_server_policies lookup failed"
                );
                return;
            }
        };

        let runtime: String = policy.try_get("runtime").unwrap_or_default();
        if runtime.is_empty() {
            return;
        }
        let primary_server: String = policy.try_get("primary_server").unwrap_or_default();

        let updated = sqlx::query(
            "UPDATE fleet_workers SET runtime = $1, updated_at = NOW() \
             WHERE name = $2 AND runtime IS DISTINCT FROM $1",
        )
        .bind(&runtime)
        .bind(&beat.computer_name)
        .execute(&self.pg)
        .await;
        match updated {
            Ok(r) if r.rows_affected() > 0 => {
                info!(
                    computer = %beat.computer_name,
                    arch, gpu_kind, discrete, tier,
                    runtime = %runtime,
                    server = %primary_server,
                    "server policy: classified node; fleet_workers.runtime set"
                );
            }
            Ok(_) => return, // already conformant — nothing to seed
            Err(e) => {
                warn!(
                    computer = %beat.computer_name,
                    error = %e,
                    "server policy: fleet_workers.runtime self-heal failed"
                );
                return;
            }
        }

        // Seed model downloads for the freshly classified node.
        let seed_json: String = policy.try_get("seed_ids").unwrap_or_default();
        let seed_ids: Vec<String> = serde_json::from_str(&seed_json).unwrap_or_default();
        for id in seed_ids {
            // The id is interpolated into a shell command run by the node's
            // defer-worker — refuse anything but plain catalog-id characters.
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                warn!(model = %id, "server policy: refusing non-catalog-id seed value");
                continue;
            }
            self.seed_model_download(&beat.computer_name, &id).await;
        }
    }

    /// Enqueue `ff model download <catalog_id>` on `node` through the
    /// deferred task queue (same fleet_tasks row shape as
    /// `ff_db::pg_enqueue_deferred`, which ff-pulse can't call directly —
    /// no ff-db dependency). Skips silently if the node's library already
    /// has the model or an open seed task for it exists.
    async fn seed_model_download(&self, node: &str, catalog_id: &str) {
        let in_library = sqlx::query(
            "SELECT 1 AS one FROM fleet_model_library \
             WHERE worker_name = $1 AND catalog_id = $2 LIMIT 1",
        )
        .bind(node)
        .bind(catalog_id)
        .fetch_optional(&self.pg)
        .await;
        if !matches!(in_library, Ok(None)) {
            return; // present already, or lookup failed — don't enqueue blind
        }

        let title = format!("Seed model {catalog_id} on {node}");
        let open_task = sqlx::query(
            "SELECT 1 AS one FROM fleet_tasks \
             WHERE task_class = 'deferred' AND summary = $1 \
               AND status NOT IN ('completed', 'failed', 'cancelled') \
             LIMIT 1",
        )
        .bind(&title)
        .fetch_optional(&self.pg)
        .await;
        if !matches!(open_task, Ok(None)) {
            return;
        }

        let command = format!("ff model download {catalog_id}");
        let inserted = sqlx::query(
            "INSERT INTO fleet_tasks \
                (task_type, summary, payload, priority, requires_capability, \
                 status, created_at, task_class) \
             VALUES ( \
                 'shell', $1, \
                 jsonb_build_object( \
                     'deferred_payload', jsonb_build_object('command', $2), \
                     'created_by', 'pulse-materializer', \
                     'kind', 'shell', \
                     'trigger_type', 'now', \
                     'trigger_spec', '{}'::jsonb, \
                     'preferred_node', $3, \
                     'required_caps', '[]'::jsonb, \
                     'attempts', 0, \
                     'max_attempts', 3 \
                 ), \
                 50, '[]'::jsonb, 'pending', NOW(), 'deferred')",
        )
        .bind(&title)
        .bind(&command)
        .bind(node)
        .execute(&self.pg)
        .await;
        match inserted {
            Ok(_) => info!(
                computer = %node,
                model = %catalog_id,
                "server policy: seeded model download via deferred task"
            ),
            Err(e) => warn!(
                computer = %node,
                model = %catalog_id,
                error = %e,
                "server policy: model download seed enqueue failed"
            ),
        }
    }
}

// -----------------------------------------------------------------------------
// Small helpers
// -----------------------------------------------------------------------------

/// What a status transition must write, decided once from the Q1 read.
/// The actual flip is executed as a compare-and-set on `prev_status`, so
/// a plan is only ever applied if the row still holds the status it was
/// planned against.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusTransitionPlan {
    /// Status the row moves to ("online" / "offline").
    pub new_status: &'static str,
    /// Open a computer_downtime_events row (offline transition).
    pub insert_downtime: bool,
    /// Close the open downtime event (return from a down state).
    pub close_downtime: bool,
    /// Publish `fleet:node_online` to wake the deferred-task scheduler.
    pub publish_node_online: bool,
}

/// Pure transition planner: returns None when the beat doesn't change the
/// row status. Extracted so the transition decision table is unit-testable
/// without a database.
fn plan_status_transition(prev_status: &str, going_offline: bool) -> Option<StatusTransitionPlan> {
    let new_status = if going_offline { "offline" } else { "online" };
    if prev_status == new_status {
        return None;
    }
    if going_offline {
        Some(StatusTransitionPlan {
            new_status,
            insert_downtime: true,
            close_downtime: false,
            publish_node_online: false,
        })
    } else {
        let was_down = matches!(prev_status, "offline" | "sdown" | "odown");
        Some(StatusTransitionPlan {
            new_status,
            insert_downtime: false,
            close_downtime: was_down,
            publish_node_online: was_down,
        })
    }
}

/// Whitespace-normalize a JSON string so two logically-equal JSON values
/// compare equal even if Postgres reformatted its copy.
fn normalize_json(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => v.to_string(),
        Err(_) => s.to_string(),
    }
}

/// Whether the persistent content of the `computers` row changed (vs a pure
/// `last_seen_at` refresh). Since the delta-path write became one atomic
/// statement this no longer gates the write itself — Q4 always carries every
/// persistent field — it feeds `ProcessReport.wrote_computer_row` so drift
/// stays observable. Extracted as a pure function so the drift invariants
/// are unit-testable — in particular that a changed `primary_ip` ALONE
/// counts as a rewrite. That signal matters: `computers.primary_ip` once
/// froze to a node's dead wifi address (aura, 2026-04-28) and `fleet_workers.ip`
/// drifted on 9/15 computers undetected for weeks, because the drift signal was
/// missing from the write condition. This guard keeps `primary_ip` in the OR so
/// the regression can't return silently.
fn persistent_fields_changed(
    ips_differ: bool,
    hw_differ: bool,
    cap_differ: bool,
    primary_ip_differ: bool,
) -> bool {
    ips_differ || hw_differ || cap_differ || primary_ip_differ
}

fn can_use_last_seen_fast_path(
    snapshots_match: bool,
    status_changed: bool,
    persistent_row_differ: bool,
) -> bool {
    snapshots_match && !status_changed && !persistent_row_differ
}

fn computer_row_has_empty_node_attributes(primary_ip: Option<&str>, ram_gb: Option<i32>) -> bool {
    primary_ip.is_none_or(str::is_empty) || ram_gb.is_none_or(|ram| ram <= 0)
}

/// Names of the persistent `computers` columns whose incoming beat value is
/// empty/zero. Used only for observability: a beat that fails
/// `computer_row_has_empty_node_attributes` (empty primary_ip or non-positive
/// ram) is rejected upstream, but the remaining persistent columns
/// (`all_ips`, `cpu_cores`, `total_disk_gb`, `gpu_kind`) can still be written
/// empty by a partially-probed beat — a daemon that finished IP+RAM probing
/// but not disk/GPU enumeration. Those transient empty writes were previously
/// invisible; logging the exact column set makes the "row briefly went empty"
/// class of drift traceable to the beat that caused it.
fn empty_persistent_beat_fields(beat: &PulseBeatV2, all_ips_json: &str) -> Vec<&'static str> {
    let mut empty = Vec::new();
    if beat.network.primary_ip.is_empty() {
        empty.push("primary_ip");
    }
    if normalize_json(all_ips_json) == "[]" {
        empty.push("all_ips");
    }
    if beat.hardware.cpu_cores <= 0 {
        empty.push("cpu_cores");
    }
    if beat.hardware.ram_gb <= 0 {
        empty.push("total_ram_gb");
    }
    if beat.hardware.disk_gb <= 0 {
        empty.push("total_disk_gb");
    }
    if beat.capabilities.gpu_kind.is_empty() {
        empty.push("gpu_kind");
    }
    empty
}

/// RAM tier key for server-policy resolution: <=8GB is `tiny` (CPU-only
/// llama-server, no model seed), everything else `standard`. Callers must
/// gate out non-positive ram_gb (degenerate beats) before classifying.
fn ram_tier(ram_gb: i32) -> &'static str {
    if ram_gb <= 8 { "tiny" } else { "standard" }
}

/// Whether the GPU owns a discrete VRAM pool, vs sharing system RAM.
/// Mirrors the autoscaler's memory-pool classifier: GB10 DGX Sparks
/// (os_family `linux-dgx`) and Apple Silicon are unified; AMD ROCm
/// reporting only a tiny (<8GB) VRAM carve-out is GTT-unified (Strix Halo,
/// where the 2GB "VRAM" is a carve-out of the 123GB RAM pool).
fn has_discrete_vram(gpu_kind: &str, os_family: &str, gpu_total_vram_gb: Option<f64>) -> bool {
    match gpu_kind {
        "nvidia_cuda" => os_family != "linux-dgx",
        "amd_rocm" => gpu_total_vram_gb.unwrap_or(0.0) >= 8.0,
        _ => false, // apple_silicon = unified; none/integrated = no VRAM
    }
}

/// Arch key for server-policy resolution. Prefers the beat's self-reported
/// `os.arch` (V165+ daemons); beats from older daemons fall back to a
/// derivation — the only aarch64 hosts in the fleet without the field are
/// DGX Sparks (linux-dgx) and Apple Silicon Macs.
fn resolve_arch(os_arch: &str, os_family: &str, gpu_kind: &str) -> String {
    if !os_arch.is_empty() {
        return os_arch.to_string();
    }
    if os_family == "linux-dgx" || gpu_kind == "apple_silicon" {
        "aarch64".to_string()
    } else {
        "x86_64".to_string()
    }
}

// -----------------------------------------------------------------------------
// Unit tests — decision logic only; no real DB.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beat_v2::{
        Capabilities, ClusterInfo, DockerContainer, DockerProject, DockerStatus, HardwareInfo,
        InstalledSoftware, LlmMemoryUsage, LlmServer, LlmServerModel, NetworkInfo,
    };
    use chrono::Utc;

    fn beat_online(name: &str) -> PulseBeatV2 {
        let mut b = PulseBeatV2::skeleton(name);
        b.going_offline = false;
        b.network = NetworkInfo {
            primary_ip: "10.0.0.1".to_string(),
            all_ips: vec![],
        };
        b.hardware = HardwareInfo {
            cpu_cores: 8,
            ram_gb: 32,
            disk_gb: 500,
            gpu: None,
        };
        b.capabilities = Capabilities {
            can_serve_ff_gateway: true,
            can_serve_openclaw_gateway: false,
            can_host_postgres_replica: false,
            can_host_redis_replica: false,
            gpu_kind: "apple_silicon".to_string(),
            gpu_count: 1,
            gpu_vram_gb: Some(96.0),
            gpu_total_vram_gb: Some(96.0),
            can_run_cuda: false,
            can_run_metal: true,
            can_run_rocm: false,
            recommended_runtimes: vec!["mlx".to_string()],
            max_runnable_model_gb: Some(80.0),
        };
        b
    }

    #[test]
    fn snapshot_detects_status_transition_offline_to_online() {
        let beat = beat_online("taylor");
        let snap = PersistedSnapshot::from_beat(&beat);
        assert_eq!(snap.status, "online");

        let mut beat2 = beat_online("taylor");
        beat2.going_offline = true;
        let snap2 = PersistedSnapshot::from_beat(&beat2);
        assert_eq!(snap2.status, "offline");

        // Different status → snapshots are not equal → materializer will write.
        assert_ne!(snap, snap2);
    }

    #[test]
    fn snapshot_equality_when_only_ephemeral_fields_change() {
        // Two beats identical in the persistent subset but with different
        // ephemeral fields should produce equal snapshots.
        let mut a = beat_online("marcus");
        let mut b = beat_online("marcus");

        // Ephemeral: tokens/sec, queue depth, load info.
        a.load.cpu_pct = 10.0;
        b.load.cpu_pct = 85.0;
        a.load.active_inference_requests = 0;
        b.load.active_inference_requests = 42;

        let sa = PersistedSnapshot::from_beat(&a);
        let sb = PersistedSnapshot::from_beat(&b);
        assert_eq!(
            sa, sb,
            "persistent snapshot must ignore ephemeral load fields"
        );
    }

    #[test]
    fn snapshot_inequality_when_installed_version_changes() {
        let mut a = beat_online("priya");
        let mut b = beat_online("priya");

        a.installed_software = vec![InstalledSoftware {
            id: "ff".to_string(),
            version: "2026.4.6".to_string(),
            install_source: Some("direct".to_string()),
            install_path: Some("~/.local/bin/ff".to_string()),
            metadata: None,
        }];
        b.installed_software = vec![InstalledSoftware {
            id: "ff".to_string(),
            version: "2026.4.7".to_string(),
            install_source: Some("direct".to_string()),
            install_path: Some("~/.local/bin/ff".to_string()),
            metadata: None,
        }];

        let sa = PersistedSnapshot::from_beat(&a);
        let sb = PersistedSnapshot::from_beat(&b);
        assert_ne!(sa, sb, "version bump must invalidate snapshot");
    }

    #[test]
    fn snapshot_captures_docker_containers() {
        let mut a = beat_online("sophie");
        a.docker = DockerStatus {
            daemon_running: true,
            total_cpu_pct: 5.0,
            total_memory_mb: 1024.0,
            memory_limit_mb: 8192.0,
            projects: vec![DockerProject {
                name: "forgefleet".to_string(),
                compose_file: Some("docker-compose.yml".to_string()),
                status: "running".to_string(),
                containers: vec![DockerContainer {
                    name: "postgres".to_string(),
                    container_id: "abc123".to_string(),
                    image: "postgres:16".to_string(),
                    ports: vec!["5432:5432".to_string()],
                    status: "running".to_string(),
                    health: Some("healthy".to_string()),
                    cpu_pct: 1.0,
                    memory_mb: 256.0,
                    memory_limit_mb: 1024.0,
                    uptime_sec: 3600,
                }],
            }],
        };

        let snap = PersistedSnapshot::from_beat(&a);
        assert_eq!(snap.docker_containers.len(), 1);
        assert_eq!(snap.docker_containers[0].container_name, "postgres");
        assert_eq!(
            snap.docker_containers[0].project_name.as_deref(),
            Some("forgefleet")
        );
    }

    #[test]
    fn snapshot_ignores_ephemeral_container_metrics() {
        let mut a = beat_online("james");
        let mut b = beat_online("james");
        let base_container = DockerContainer {
            name: "redis".to_string(),
            container_id: "id1".to_string(),
            image: "redis:7".to_string(),
            ports: vec!["6379:6379".to_string()],
            status: "running".to_string(),
            health: Some("healthy".to_string()),
            cpu_pct: 2.0,
            memory_mb: 100.0,
            memory_limit_mb: 512.0,
            uptime_sec: 60,
        };

        let mut ca = base_container.clone();
        let mut cb = base_container.clone();
        ca.cpu_pct = 2.0;
        cb.cpu_pct = 99.0; // ephemeral
        ca.uptime_sec = 60;
        cb.uptime_sec = 99999; // ephemeral
        ca.memory_mb = 100.0;
        cb.memory_mb = 500.0; // ephemeral

        a.docker = DockerStatus {
            daemon_running: true,
            total_cpu_pct: 0.0,
            total_memory_mb: 0.0,
            memory_limit_mb: 0.0,
            projects: vec![DockerProject {
                name: "forgefleet".to_string(),
                compose_file: None,
                status: "running".to_string(),
                containers: vec![ca],
            }],
        };
        b.docker = DockerStatus {
            daemon_running: true,
            total_cpu_pct: 50.0,
            total_memory_mb: 200.0,
            memory_limit_mb: 8192.0,
            projects: vec![DockerProject {
                name: "forgefleet".to_string(),
                compose_file: None,
                status: "running".to_string(),
                containers: vec![cb],
            }],
        };

        let sa = PersistedSnapshot::from_beat(&a);
        let sb = PersistedSnapshot::from_beat(&b);
        assert_eq!(
            sa, sb,
            "ephemeral per-container CPU/memory/uptime must not affect snapshot"
        );
    }

    #[test]
    fn snapshot_captures_llm_deployment_persistent_fields_only() {
        let mut a = beat_online("taylor");
        let mut b = beat_online("taylor");
        let dep_id = Uuid::new_v4();
        let started = Utc::now();

        let base = LlmServer {
            deployment_id: dep_id,
            runtime: "llama.cpp".to_string(),
            endpoint: "http://10.0.0.1:51001".to_string(),
            openai_compatible: true,
            model: LlmServerModel {
                id: "qwen3-coder-30b".to_string(),
                display_name: "Qwen3 Coder 32B".to_string(),
                loaded_path: "/models/qwen.gguf".to_string(),
                context_window: 32768,
                parallel_slots: 4,
            },
            status: "active".to_string(),
            pid: Some(12345),
            started_at: started,
            cluster: ClusterInfo {
                cluster_id: None,
                role: "solo".to_string(),
                tensor_parallel_size: 1,
                pipeline_parallel_size: 1,
                peers: vec![],
            },
            queue_depth: 0,
            active_requests: 0,
            tokens_per_sec_last_min: 0.0,
            gpu_memory_used_gb: Some(20.0),
            is_healthy: true,
            last_probed_at: started,
            memory_used: LlmMemoryUsage {
                model_weights_gb: 18.0,
                kv_cache_gb: 1.5,
                overhead_gb: 0.5,
                total_gb: 20.0,
            },
        };

        let mut sa_srv = base.clone();
        let mut sb_srv = base.clone();
        // Ephemeral:
        sa_srv.queue_depth = 0;
        sb_srv.queue_depth = 42;
        sa_srv.active_requests = 0;
        sb_srv.active_requests = 99;
        sa_srv.tokens_per_sec_last_min = 0.0;
        sb_srv.tokens_per_sec_last_min = 250.0;
        sa_srv.is_healthy = true;
        sb_srv.is_healthy = true;

        a.llm_servers = vec![sa_srv];
        b.llm_servers = vec![sb_srv];

        let sa = PersistedSnapshot::from_beat(&a);
        let sb = PersistedSnapshot::from_beat(&b);
        assert_eq!(
            sa, sb,
            "queue_depth/active_requests/tokens_per_sec must be excluded from persistent snapshot"
        );
    }

    #[test]
    fn normalize_json_equality() {
        let a = "[{\"iface\":\"en0\",\"ip\":\"10.0.0.1\",\"kind\":\"v4\"}]";
        let b = "[ { \"iface\" : \"en0\" , \"ip\" : \"10.0.0.1\" , \"kind\" : \"v4\" } ]";
        assert_eq!(normalize_json(a), normalize_json(b));
    }

    #[test]
    fn primary_ip_drift_alone_forces_a_row_rewrite() {
        // The item-21 invariant: a changed primary_ip must trigger a persistent
        // write even when IPs/hardware/capabilities are otherwise identical.
        // Regression guard for the aura wifi→ethernet freeze + the 9/15
        // fleet_workers.ip drift.
        assert!(persistent_fields_changed(false, false, false, true));
    }

    #[test]
    fn detects_empty_computer_row_node_attributes() {
        assert!(computer_row_has_empty_node_attributes(None, Some(32)));
        assert!(computer_row_has_empty_node_attributes(Some(""), Some(32)));
        assert!(computer_row_has_empty_node_attributes(
            Some("10.0.0.1"),
            None
        ));
        assert!(computer_row_has_empty_node_attributes(
            Some("10.0.0.1"),
            Some(0)
        ));
        assert!(!computer_row_has_empty_node_attributes(
            Some("10.0.0.1"),
            Some(32)
        ));
    }

    #[test]
    fn empty_persistent_beat_fields_flags_partial_probes() {
        // A fully-probed online beat has no empty persistent columns.
        let mut good = beat_online("marcus");
        good.network.all_ips = vec![crate::beat_v2::Ip {
            iface: "en0".to_string(),
            ip: "10.0.0.1".to_string(),
            kind: "v4".to_string(),
            paired_with: None,
            link_speed_gbps: None,
            medium: None,
        }];
        let good_ips = serde_json::to_string(&good.network.all_ips).unwrap();
        assert!(empty_persistent_beat_fields(&good, &good_ips).is_empty());

        // A partially-probed beat: IP+RAM present (so it isn't rejected
        // upstream) but disk/GPU enumeration hasn't finished. Those columns
        // must be reported so the transient empty write is traceable.
        let mut partial = beat_online("marcus");
        partial.hardware.disk_gb = 0;
        partial.capabilities.gpu_kind = String::new();
        let partial_ips = serde_json::to_string(&partial.network.all_ips).unwrap();
        let fields = empty_persistent_beat_fields(&partial, &partial_ips);
        assert!(fields.contains(&"total_disk_gb"));
        assert!(fields.contains(&"gpu_kind"));
        assert!(!fields.contains(&"primary_ip"));
        assert!(!fields.contains(&"total_ram_gb"));

        // An empty all_ips array is flagged (`[]` normalizes to `[]`).
        assert!(empty_persistent_beat_fields(&partial, "[]").contains(&"all_ips"));
    }

    #[test]
    fn no_rewrite_when_nothing_changed() {
        assert!(!persistent_fields_changed(false, false, false, false));
    }

    #[test]
    fn matching_redis_snapshot_does_not_hide_postgres_row_drift() {
        assert!(!can_use_last_seen_fast_path(true, false, true));
        assert!(can_use_last_seen_fast_path(true, false, false));
    }

    #[test]
    fn computers_row_upsert_is_one_atomic_statement() {
        // The delta-path computers write must stay a SINGLE statement — no
        // DELETE+INSERT, no read-then-branch partial UPDATE — so concurrent
        // writers can never interleave between deciding what to write and
        // writing it.
        let sql = UPSERT_COMPUTER_ROW_SQL;
        assert!(!sql.contains(';'), "must be a single statement");
        assert!(sql.trim_start().starts_with("UPDATE computers SET"));
        assert!(!sql.contains("DELETE"), "must not delete-then-insert");
        assert!(!sql.contains("INSERT"), "row creation is enrollment's job");
    }

    #[test]
    fn computers_row_upsert_carries_every_persistent_field() {
        // Idempotency guard: the atomic statement must assign every
        // persistent column unconditionally so replaying the same beat
        // always converges to the same row, regardless of what a prior
        // (possibly stale) read believed had changed.
        for col in [
            "last_seen_at",
            "primary_ip",
            "all_ips",
            "cpu_cores",
            "total_ram_gb",
            "total_disk_gb",
            "gpu_kind",
            "gpu_count",
            "gpu_total_vram_gb",
            "has_gpu",
        ] {
            assert!(
                UPSERT_COMPUTER_ROW_SQL.contains(col),
                "atomic computers upsert must set {col}"
            );
        }
    }

    #[test]
    fn any_single_drift_signal_forces_a_rewrite() {
        assert!(persistent_fields_changed(true, false, false, false)); // ips
        assert!(persistent_fields_changed(false, true, false, false)); // hardware
        assert!(persistent_fields_changed(false, false, true, false)); // capabilities
        assert!(persistent_fields_changed(false, false, false, true)); // primary_ip
    }

    #[test]
    fn status_transition_offline_to_online_detected() {
        // Simulate the decision the materializer would make: if prev_status
        // is "offline" and new beat is not going_offline, we need a status
        // change entry in the report and a close-downtime-event write.
        let prev_status = "offline".to_string();
        let beat = beat_online("ace");
        let new_status = if beat.going_offline {
            "offline"
        } else {
            "online"
        }
        .to_string();
        assert_ne!(prev_status, new_status);
        assert!(matches!(
            prev_status.as_str(),
            "offline" | "sdown" | "odown"
        ));
    }

    #[test]
    fn transition_plan_none_when_status_unchanged() {
        assert!(plan_status_transition("online", false).is_none());
        assert!(plan_status_transition("offline", true).is_none());
    }

    #[test]
    fn transition_plan_offline_opens_downtime_event_only() {
        let plan = plan_status_transition("online", true).expect("transition");
        assert_eq!(plan.new_status, "offline");
        assert!(plan.insert_downtime);
        assert!(!plan.close_downtime);
        assert!(!plan.publish_node_online);
    }

    #[test]
    fn transition_plan_return_from_down_states_closes_downtime_and_publishes() {
        for prev in ["offline", "sdown", "odown"] {
            let plan = plan_status_transition(prev, false).expect("transition");
            assert_eq!(plan.new_status, "online");
            assert!(!plan.insert_downtime);
            assert!(plan.close_downtime, "prev={prev} must close downtime");
            assert!(plan.publish_node_online, "prev={prev} must publish");
        }
    }

    #[test]
    fn transition_plan_online_from_non_down_state_skips_downtime_close() {
        // e.g. an enrolling/unknown row coming online for the first time:
        // flip the status, but there is no open downtime event to close and
        // no node_online wake to publish.
        let plan = plan_status_transition("enrolling", false).expect("transition");
        assert_eq!(plan.new_status, "online");
        assert!(!plan.insert_downtime);
        assert!(!plan.close_downtime);
        assert!(!plan.publish_node_online);
    }

    #[test]
    fn skip_writes_when_snapshot_matches_and_status_unchanged() {
        // If prior snapshot equals current snapshot AND status didn't
        // change, the materializer takes the fast path and only issues
        // one UPDATE.
        let a = beat_online("marcus");
        let prior = PersistedSnapshot::from_beat(&a);
        let current = PersistedSnapshot::from_beat(&a);
        let status_changed = false;
        let snapshots_match = prior == current;
        assert!(snapshots_match);
        assert!(!status_changed);
        // Expected behavior: exactly one write: UPDATE computers SET last_seen_at.
    }

    #[test]
    fn ram_tier_splits_at_8gb() {
        assert_eq!(ram_tier(3), "tiny");
        assert_eq!(ram_tier(8), "tiny");
        assert_eq!(ram_tier(9), "standard");
        assert_eq!(ram_tier(123), "standard");
    }

    #[test]
    fn discrete_vram_classification_matches_fleet_hardware() {
        // DGX Spark GB10: CUDA but unified memory.
        assert!(!has_discrete_vram("nvidia_cuda", "linux-dgx", Some(122.0)));
        // Generic x86 NVIDIA box: discrete VRAM.
        assert!(has_discrete_vram("nvidia_cuda", "linux-ubuntu", Some(24.0)));
        // Strix Halo (duncan/lily/logan/veronica): 2.1GB carve-out → GTT-unified.
        assert!(!has_discrete_vram(
            "amd_rocm",
            "linux-ubuntu",
            Some(2.147483648)
        ));
        assert!(!has_discrete_vram("amd_rocm", "linux-ubuntu", None));
        // AMD with a real discrete card.
        assert!(has_discrete_vram("amd_rocm", "linux-ubuntu", Some(24.0)));
        // Apple Silicon and CPU-only hosts never have discrete VRAM.
        assert!(!has_discrete_vram("apple_silicon", "macos", Some(96.0)));
        assert!(!has_discrete_vram("none", "linux-ubuntu", None));
    }

    #[test]
    fn arch_prefers_beat_field_then_derives_from_family() {
        assert_eq!(resolve_arch("aarch64", "linux-ubuntu", "none"), "aarch64");
        assert_eq!(resolve_arch("x86_64", "linux-dgx", "nvidia_cuda"), "x86_64");
        // Pre-V165 daemons: no arch in the beat — derive.
        assert_eq!(resolve_arch("", "linux-dgx", "nvidia_cuda"), "aarch64");
        assert_eq!(resolve_arch("", "macos", "apple_silicon"), "aarch64");
        assert_eq!(resolve_arch("", "linux-ubuntu", "amd_rocm"), "x86_64");
    }
}
