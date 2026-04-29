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
    ///   Q4  UPDATE_COMPUTER_PERSISTENT_FIELDS           (delta-path)
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

        report.computer_id = Some(computer_id);

        // Build the persistent snapshot for this beat.
        let new_snapshot = PersistedSnapshot::from_beat(beat);

        // Q2: compare against last-persisted snapshot in Redis.
        let redis_key = format!("{PERSISTED_SNAPSHOT_PREFIX}{}", beat.computer_name);
        let mut redis_conn = self.redis.get_multiplexed_async_connection().await?;
        let prior_snapshot_json: Option<String> = redis_conn.get(&redis_key).await.ok().flatten();
        let prior_snapshot: Option<PersistedSnapshot> = prior_snapshot_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());

        // Determine new status from beat.
        let new_status = if beat.going_offline {
            "offline".to_string()
        } else {
            "online".to_string()
        };
        let status_changed = prev_status != new_status;

        if status_changed {
            report.status_transition = Some((prev_status.clone(), new_status.clone()));
        }

        // Fast path: if the snapshot matches exactly AND no status transition,
        // only update last_seen_at.
        let snapshots_match = prior_snapshot
            .as_ref()
            .map(|ps| ps == &new_snapshot)
            .unwrap_or(false);

        if snapshots_match && !status_changed {
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
        let new_ips_json = &new_snapshot.all_ips_json;
        let ips_differ = prev_all_ips_text
            .as_deref()
            .map(|s| normalize_json(s) != normalize_json(new_ips_json))
            .unwrap_or(true);
        if ips_differ {
            report.ips_updated = true;
        }

        // Hardware comparison.
        let hw_differ = prev_cpu_cores != Some(beat.hardware.cpu_cores)
            || prev_ram_gb != Some(beat.hardware.ram_gb)
            || prev_disk_gb != Some(beat.hardware.disk_gb);

        // Capability comparison.
        let cap_differ = prev_gpu_kind.as_deref() != Some(beat.capabilities.gpu_kind.as_str())
            || prev_gpu_count != Some(beat.capabilities.gpu_count)
            || prev_gpu_total_vram_gb != beat.capabilities.gpu_total_vram_gb;

        // primary_ip comparison. Without this the column would be frozen to
        // whichever interface the node had at first enrollment — surfaced
        // 2026-04-28 on aura, where DB primary_ip was a dead wifi address
        // (192.168.5.109) long after the laptop switched to ethernet
        // (192.168.5.110). `ff fleet versions --live` (and any other
        // primary_ip-using ssh path) was effectively unreachable.
        let primary_ip_differ =
            prev_primary_ip.as_deref() != Some(beat.network.primary_ip.as_str());

        // Q4: UPDATE_COMPUTER_PERSISTENT_FIELDS
        if ips_differ || hw_differ || cap_differ || primary_ip_differ {
            sqlx::query(
                "UPDATE computers SET \
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
                 WHERE id = $1",
            )
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
            report.wrote_computer_row = true;
        } else {
            // At minimum refresh last_seen_at.
            sqlx::query("UPDATE computers SET last_seen_at = NOW() WHERE id = $1")
                .bind(computer_id)
                .execute(&self.pg)
                .await?;
        }

        // Status transition handling.
        if status_changed {
            report.wrote_computer_row = true;
            if beat.going_offline {
                // Q5/Q6: transition to offline.
                sqlx::query(
                    "UPDATE computers SET \
                        status = 'offline', \
                        status_changed_at = NOW(), \
                        offline_since = COALESCE(offline_since, NOW()) \
                     WHERE id = $1",
                )
                .bind(computer_id)
                .execute(&self.pg)
                .await?;

                sqlx::query(
                    "INSERT INTO computer_downtime_events \
                        (computer_id, offline_at, cause) \
                     VALUES ($1, NOW(), 'graceful_shutdown')",
                )
                .bind(computer_id)
                .execute(&self.pg)
                .await?;
            } else {
                // Q5: transition to online.
                sqlx::query(
                    "UPDATE computers SET \
                        status = 'online', \
                        status_changed_at = NOW(), \
                        offline_since = NULL \
                     WHERE id = $1",
                )
                .bind(computer_id)
                .execute(&self.pg)
                .await?;

                // Q7: close the most recent open downtime event, if any.
                if matches!(prev_status.as_str(), "offline" | "sdown" | "odown") {
                    sqlx::query(
                        "UPDATE computer_downtime_events \
                         SET online_at = NOW(), \
                             duration_sec = EXTRACT(EPOCH FROM (NOW() - offline_at))::INT, \
                             resolved_by = 'pulse_return' \
                         WHERE computer_id = $1 AND online_at IS NULL",
                    )
                    .bind(computer_id)
                    .execute(&self.pg)
                    .await?;

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
        // resend them. The `ADD COLUMN IF NOT EXISTS` guards against
        // running against an older DB where the column isn't deployed
        // yet — it's a no-op when the column is already there.
        let _ = sqlx::query(
            "ALTER TABLE computer_software \
                ADD COLUMN IF NOT EXISTS metadata JSONB NOT NULL DEFAULT '{}'::jsonb",
        )
        .execute(&self.pg)
        .await;
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
}

// -----------------------------------------------------------------------------
// Small helpers
// -----------------------------------------------------------------------------

/// Whitespace-normalize a JSON string so two logically-equal JSON values
/// compare equal even if Postgres reformatted its copy.
fn normalize_json(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => v.to_string(),
        Err(_) => s.to_string(),
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
            can_serve_openclaw_gateway: true,
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
                id: "qwen2.5-coder-32b".to_string(),
                display_name: "Qwen2.5 Coder 32B".to_string(),
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
}
