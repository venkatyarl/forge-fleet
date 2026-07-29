//! Deployment reconciler — drive live state toward DB desired_state.
//!
//! Runs every 60s inside `ff daemon`. Compares the local process snapshot
//! against `fleet_model_deployments` rows for this worker and reconciles in
//! both directions:
//!
//!   - Process running, no DB row                → adopt (insert row)
//!   - Both present                              → refresh last_health + status
//!   - DB row present (desired='active'), no proc → RESPAWN via load_model
//!   - DB row present (desired='retired'), proc   → kill the process
//!   - DB row present (desired='retired'), no proc → delete the row
//!
//! Before V90 the reconciler only adopted live processes (one-way: live → DB).
//! When a spawned llama-server died, the next tick would delete the row, so
//! "the operator wanted this LLM up" was forgotten. After V90, `desired_state`
//! survives a missing process and this reconciler reads it.

use std::collections::HashMap;
use std::path::Path;

/// Canonical inference ports per the fleet port registry ([[canonical-ports]]):
/// llama.cpp / mlx slots are 55000-55010, vllm uses 51001 / 51003, ollama 11434.
/// A process on any OTHER port is a stray candidate — but it is only reaped when
/// no `active` deployment row claims that port (see the Pass-A guard). Operator
/// intent expressed via `ff model load` is authoritative and must survive on any
/// port; the earlier 55000-55010-only window wrongly reaped vllm/ollama endpoints
/// and any agent endpoint warmed on the `ff model load` default port (51001).
pub const CANONICAL_PORT_MIN: i32 = 55000;
pub const CANONICAL_PORT_MAX: i32 = 55010;

fn port_is_canonical(port: i32) -> bool {
    (CANONICAL_PORT_MIN..=CANONICAL_PORT_MAX).contains(&port)
        || matches!(port, 51001 | 51003 | 11434)
}

/// Summary of a reconcile pass.
#[derive(Debug, Clone, Default)]
pub struct ReconcileSummary {
    /// Existing processes that were newly inserted into the DB.
    pub adopted: usize,
    /// DB rows removed because the process was gone and desired_state='retired'.
    pub removed: usize,
    /// Existing rows whose health_status was refreshed.
    pub refreshed: usize,
    /// Dead 'active' deployments that were respawned this tick.
    pub respawned: usize,
    /// Dead 'active' deployments whose missing library_id was recovered from
    /// catalog_id, then respawned (a row that would otherwise have been reaped).
    pub recovered: usize,
    /// Dead 'active' deployments permanently reaped because no library link
    /// could be established (truly un-respawnable). Distinct from `removed`
    /// (retired rows) — a reap here means an endpoint the operator wanted up was
    /// removed and CANNOT come back without a fresh `ff model load`. Logged at
    /// WARN so a vanished agent endpoint never disappears silently again.
    pub reaped: usize,
    /// Stray processes for 'retired' deployments that were killed.
    pub killed: usize,
    /// Non-canonical port violations flipped to desired_state='retired' for
    /// removal on the same pass.
    pub port_violations: usize,
}

/// Run one reconcile pass. Returns counts for logging.
/// Restart LOCAL model deployments that are ALIVE-but-unhealthy (503-hung) — the
/// gap the base reconciler misses (it respawns DEAD processes, not hung ones, so
/// a 503-wedged server like the 5.6-day-stuck 480B is never fixed). This is
/// "ff fixes the LLM": unload + reload (load_model health-waits), returning the
/// model to rotation once it answers. Runs per-node, so each computer's
/// orchestrator fixes its OWN models (operator's model 2026-07-25: llm router →
/// leader → local orchestrator fixes + tests → back in rotation).
///
/// SAFETY: gated on the `deployment_autorestart_mode` fleet-secret — default
/// **OFF** so a fresh fleet-wide deploy can't restart-storm every node at once;
/// flip to "on" deliberately. Skips the multi-node 480B RING (`%480b%` — it needs
/// its RPC ring recipe, not a plain reload). `started_at` gives a natural 15-min
/// cooldown so a freshly-(re)started/loading server isn't restarted again.
fn autorestart_enabled(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "on" | "true" | "1"))
}

const AUTORESTART_HEALTH_STATUS: &str = "unhealthy";

#[cfg(test)]
fn health_requires_autorestart(status: &str) -> bool {
    status == AUTORESTART_HEALTH_STATUS
}

fn restart_spec(library_id: Option<String>, port: i32) -> Option<(String, u16)> {
    let port = u16::try_from(port).ok().filter(|port| *port != 0)?;
    Some((library_id?, port))
}

pub async fn restart_hung_local_deployments(pool: &sqlx::PgPool) -> u64 {
    let gate = ff_db::pg_read_gate_value(pool, "deployment_autorestart_mode", "off", "off").await;
    let enabled = autorestart_enabled(gate.as_deref().ok());
    if !enabled {
        return 0;
    }
    let node = crate::fleet_info::resolve_this_worker_name().await;
    let rows: Vec<(String, Option<String>, i32)> = sqlx::query_as(
        "SELECT id::text, library_id::text, port \
           FROM fleet_model_deployments \
          WHERE worker_name = $1 AND desired_state = 'active' \
            AND health_status = $2 \
            AND COALESCE(catalog_id, '') NOT ILIKE '%480b%' \
            AND (started_at IS NULL OR started_at < now() - interval '15 minutes')",
    )
    .bind(&node)
    .bind(AUTORESTART_HEALTH_STATUS)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut recovered = 0u64;
    for (id, library_id, port) in rows {
        let Some((library_id, port)) = restart_spec(library_id, port) else {
            tracing::warn!(
                deployment = %id,
                port,
                "reconciler: hung model has no valid library/port — cannot reload"
            );
            continue;
        };
        tracing::warn!(deployment = %id, port, node = %node,
            "reconciler: restarting HUNG (alive-but-unhealthy) model — ff self-heal");
        if let Err(e) = crate::model_runtime::unload_model(pool, &id).await {
            tracing::warn!(deployment = %id, error = %e, "reconciler: unload of hung model failed");
            continue;
        }
        match crate::model_runtime::load_model(
            pool,
            crate::model_runtime::LoadOptions {
                library_id,
                port,
                context_size: None,
                parallel: None,
                agent_profile: true,
                mmproj_path: None,
            },
        )
        .await
        {
            Ok(_) => {
                tracing::info!(deployment = %id, "reconciler: RECOVERED hung model → back in rotation");
                recovered += 1;
            }
            Err(e) => tracing::warn!(deployment = %id, error = %e,
                "reconciler: reload FAILED — endpoint down until next pass"),
        }
    }
    recovered
}

pub async fn reconcile_local(pool: &sqlx::PgPool) -> Result<ReconcileSummary, String> {
    let worker_name = crate::fleet_info::resolve_this_worker_name().await;

    // 1. Snapshot what's actually running on this host.
    let procs = crate::model_runtime::list_local_processes().await;

    // 2. Snapshot what the DB thinks is deployed on this host. Includes the
    //    new desired_state column from V90.
    let db_rows = list_deployments_with_desired_state(pool, &worker_name).await?;

    // Index DB rows by port for quick lookup.
    let db_by_port: HashMap<i32, &DeploymentRow> = db_rows.iter().map(|r| (r.port, r)).collect();

    let libs = ff_db::pg_list_library(pool, Some(&worker_name))
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;

    let mut summary = ReconcileSummary::default();
    let mut seen_ports: std::collections::HashSet<i32> = std::collections::HashSet::new();

    // ── Pass A — for each live process: adopt, refresh, or enforce port ──
    for proc_info in &procs {
        let Some(port) = proc_info.port else { continue };
        let port_i32 = port as i32;
        seen_ports.insert(port_i32);

        // Canonical-port enforcement. A non-canonical inference server is reaped
        // here so a stale operator-launched server (e.g. james's Qwen3.6-35B-A3B
        // on 8082 since May 2) gets cleaned up automatically — BUT ONLY when no
        // `active` deployment row claims this port. A model deliberately loaded
        // via `ff model load` (any port) is durable and must NEVER be killed or
        // retired here; doing so deleted warmed offload/agent endpoints (the
        // `ff model load --agent` the offload hint recommends defaults to 51001).
        // Excludes rpc-server / mesh helpers because list_local_processes only
        // matches llama-server / mlx_lm.server / vllm serve.
        let port_has_active_row = db_by_port
            .get(&port_i32)
            .map(|r| r.desired_state == "active")
            .unwrap_or(false);
        if !port_is_canonical(port_i32) && !port_has_active_row {
            tracing::warn!(
                pid = proc_info.pid,
                port,
                runtime = %proc_info.runtime,
                "unclaimed non-canonical listener — leaving it untouched; explicit authenticated replacement is required"
            );
            summary.port_violations += 1;
            continue;
        }

        let healthy = crate::model_runtime::probe_health_public(
            &proc_info.runtime,
            port,
            std::time::Duration::from_secs(3),
        )
        .await;
        let status = if healthy { "healthy" } else { "unhealthy" };

        if let Some(&existing) = db_by_port.get(&port_i32) {
            // ── Both DB row and process exist ─────────────────────────────
            if existing.desired_state == "retired" {
                // Retirement alone is not process identity. The explicit
                // operator unload path performs the identity-qualified stop;
                // a reconciler snapshot may be stale or the PID may be reused.
                tracing::warn!(
                    pid = proc_info.pid,
                    port,
                    "retired deployment still has a listener — refusing unauthenticated reap"
                );
                continue;
            }

            // Refresh only an already-authorized process incarnation. PID,
            // persisted OS start marker, and library/model identity must all
            // match; neither health nor a recognizable command line grants
            // adoption authority.
            let matched_library = proc_info
                .model_path
                .as_deref()
                .and_then(|path| match_library_to_path(&libs, path).0);
            let authenticated = existing.pid == Some(proc_info.pid as i32)
                && existing
                    .process_start_marker
                    .as_deref()
                    .is_some_and(|expected| {
                        crate::model_runtime::process_start_marker(proc_info.pid).as_deref()
                            == Some(expected)
                    })
                && matched_library.as_deref() == existing.library_id.as_deref();
            if !authenticated {
                tracing::warn!(
                    deployment = %existing.id,
                    pid = proc_info.pid,
                    port,
                    "live listener does not match persisted PID/start/model identity; refusing refresh or adoption"
                );
                continue;
            }

            // Refresh agent-capacity evidence from the authenticated live
            // runtime on every healthy pass. Configured/cmdline values are not
            // proof that the server accepted that profile, and old evidence
            // must expire rather than satisfying a reliability floor forever.
            let mut ctx_total: Option<i32> = None;
            let mut slots: Option<i32> = None;
            let mut usable: Option<i32> = None;
            if healthy {
                if let Some((per_slot, total_slots)) =
                    crate::model_runtime::probe_agent_ctx(&proc_info.runtime, port).await
                {
                    ctx_total = Some(per_slot.saturating_mul(total_slots));
                    slots = Some(total_slots);
                    usable = Some(per_slot);
                }
            }

            if let Err(e) = sqlx::query(
                "UPDATE fleet_model_deployments
                    SET health_status = $1,
                        last_health_at = NOW(),
                        context_window = COALESCE($4::int, context_window),
                        parallel_slots = COALESCE($5::int, parallel_slots),
                        usable_agent_ctx = COALESCE($6::int, usable_agent_ctx),
                        agent_profile_verified_at =
                            CASE WHEN $1 = 'healthy' AND $6::int IS NOT NULL
                                 THEN NOW() ELSE NULL END
                  WHERE id = $7::uuid
                    AND pid = $2
                    AND process_start_marker = $3",
            )
            .bind(status)
            .bind(proc_info.pid as i32)
            .bind(existing.process_start_marker.as_deref())
            .bind(ctx_total)
            .bind(slots)
            .bind(usable)
            .bind(&existing.id)
            .execute(pool)
            .await
            {
                tracing::warn!("failed to refresh deployment {}: {e}", existing.id);
            } else {
                summary.refreshed += 1;
                if usable.is_some() {
                    tracing::info!(
                        port,
                        usable_agent_ctx = usable,
                        "backfilled agent ctx for adopted deployment"
                    );
                }
            }
        } else {
            // ── Process exists, no DB row → foreign/unclaimed ─────────────
            tracing::warn!(
                pid = proc_info.pid,
                port,
                runtime = %proc_info.runtime,
                "unclaimed listener is not placement authority; refusing auto-adoption"
            );
        }
    }

    // ── Pass B — for each DB row whose process is gone ─────────────────────
    for row in &db_rows {
        if seen_ports.contains(&row.port) {
            continue;
        }
        match row.desired_state.as_str() {
            "retired" => {
                // Operator unloaded; row is stale. Delete.
                if let Err(e) = ff_db::pg_delete_deployment(pool, &row.id).await {
                    tracing::warn!("delete retired deployment {}: {e}", row.id);
                } else {
                    summary.removed += 1;
                }
            }
            "active" => {
                // A dead `active` row with no library_id can't be respawned as-is
                // (respawn_dead_deployment needs a library to load). Before giving
                // up, try to RECOVER the library link from the row's catalog_id:
                // a row can lose its library_id (e.g. adopted from an out-of-band
                // process before the library scan completed) while still naming a
                // catalog model, and the worker's library may now hold a matching
                // row. Recovering it turns a would-be permanent reap back into a
                // respawn — this is exactly the gap that silently lost the DGX
                // agent endpoints after a `forgefleetd` restart (2026-06-17).
                let mut row_for_respawn = row.clone();
                if dead_active_is_unrespawnable(&row_for_respawn.library_id) {
                    if let Some(lib_id) = recover_library_id(&row_for_respawn, &libs) {
                        tracing::info!(
                            port = row.port,
                            deployment = %row.id,
                            library_id = %lib_id,
                            "recovered missing library_id from catalog_id for dead active deployment — will respawn instead of reap"
                        );
                        // Persist so future ticks (and the respawn upsert) see the
                        // link even if this respawn attempt fails and retries.
                        if let Ok(uuid) = sqlx::types::Uuid::parse_str(&lib_id) {
                            let _ = sqlx::query(
                                "UPDATE fleet_model_deployments SET library_id = $1 WHERE id = $2::uuid",
                            )
                            .bind(uuid)
                            .bind(&row.id)
                            .execute(pool)
                            .await;
                        }
                        row_for_respawn.library_id = Some(lib_id);
                        summary.recovered += 1;
                    } else if row.catalog_id.as_deref().is_some_and(|c| !c.is_empty()) {
                        // Has a catalog_id but no library match RIGHT NOW. This is
                        // almost always TRANSIENT: right after a `forgefleetd`
                        // restart (every deploy triggers one) the library scanner
                        // hasn't re-registered this node's models yet, so the
                        // lookup misses for a tick or two. DELETING the row here was
                        // the real cause of the glm instability — a deploy restart
                        // killed the glm process, the next tick found the row dead +
                        // library-not-yet-scanned, and PERMANENTLY reaped the active
                        // glm deployment, so glm silently vanished until re-seeded by
                        // hand (observed fleet-wide 2026-07-26). KEEP the row and
                        // retry next tick — once the scan lands, recover_library_id
                        // succeeds and it respawns. Never destroy an operator-desired
                        // (desired_state='active') endpoint for a transient miss.
                        tracing::warn!(
                            port = row.port,
                            deployment = %row.id,
                            catalog_id = ?row.catalog_id,
                            "dead active deployment with catalog_id but no library match yet — \
                             KEEPING (library scan likely pending post-restart); will retry next tick"
                        );
                        continue;
                    } else {
                        // Truly orphaned: no library_id AND no catalog_id — nothing
                        // to ever respawn from. Reap it (a phantom 'unhealthy' row
                        // would otherwise sit in the router's candidate set forever),
                        // but at WARN with full context.
                        tracing::warn!(
                            port = row.port,
                            deployment = %row.id,
                            "reaping dead active deployment — no library_id AND no catalog_id; \
                             nothing to respawn from (restore with `ff model load <library_id>`)"
                        );
                        if let Err(e) = ff_db::pg_delete_deployment(pool, &row.id).await {
                            tracing::warn!("delete un-respawnable deployment {}: {e}", row.id);
                        } else {
                            summary.reaped += 1;
                        }
                        continue;
                    }
                }
                // Process died unexpectedly. Try to bring it back.
                match respawn_dead_deployment(pool, &row_for_respawn, &libs).await {
                    Ok(true) => summary.respawned += 1,
                    Ok(false) => {} // unable, already logged
                    Err(e) => {
                        tracing::warn!("respawn deployment {} on port {}: {e}", row.id, row.port)
                    }
                }
            }
            other => {
                tracing::warn!(
                    "unknown desired_state '{other}' for deployment {}; skipping",
                    row.id
                );
            }
        }
    }

    Ok(summary)
}

/// Whether a dead `active` deployment row needs library recovery before a
/// respawn can be attempted. A respawn loads `row.library_id`, so a row with no
/// library_id can't come back as-is — but it may be recoverable from its
/// catalog_id (see [`recover_library_id`]) before it's reaped. Pure predicate so
/// the Pass-B decision is unit-testable without a DB.
fn dead_active_is_unrespawnable(library_id: &Option<String>) -> bool {
    library_id.is_none()
}

/// Best-effort recovery of a dead deployment's missing library_id from its
/// catalog_id: find a library row on this worker that serves the same catalog
/// model. Returns the recovered library_id, or `None` when the row names no
/// catalog model or the worker has no library row for it (truly un-respawnable).
/// Pure (no DB) so the recovery decision is unit-testable. When several library
/// rows share a catalog_id, the first is taken — `load_model` resolves the
/// concrete model file under the row's path.
fn recover_library_id(row: &DeploymentRow, libs: &[ff_db::ModelLibraryRow]) -> Option<String> {
    let catalog_id = row.catalog_id.as_deref()?;
    libs.iter()
        .find(|l| l.catalog_id.as_str() == catalog_id)
        .map(|l| l.id.clone())
}

/// Resurrect a dead deployment row whose desired_state='active'. Returns
/// `Ok(true)` on successful spawn, `Ok(false)` if we couldn't (missing
/// library row, missing runtime, etc.).
async fn respawn_dead_deployment(
    pool: &sqlx::PgPool,
    row: &DeploymentRow,
    libs: &[ff_db::ModelLibraryRow],
) -> Result<bool, String> {
    let Some(lib_id) = &row.library_id else {
        tracing::warn!(
            "deployment {} desired=active but no library_id — cannot respawn",
            row.id
        );
        return Ok(false);
    };
    let Some(lib) = libs.iter().find(|l| &l.id == lib_id) else {
        tracing::warn!(
            "deployment {} references library_id {} which is gone — cannot respawn",
            row.id,
            lib_id
        );
        return Ok(false);
    };

    tracing::info!(
        port = row.port,
        library_id = %lib.id,
        "respawning dead deployment (desired_state=active)"
    );

    // NO delete-first. `respawn_model` keeps this desired row durable throughout
    // the health wait and conditionally replaces only its exact captured
    // identity at activation. A failed launch leaves intent for the next tick.
    let ctx = if row.context_window > 0 {
        row.context_window as u32
    } else {
        32_768
    };
    // Respawn with the row's recorded slot count so an agent-capable (1-slot)
    // deployment isn't silently reverted to a 4-slot split. 0 = unknown (older
    // row) → keep the historical default of 4.
    let parallel = if row.parallel_slots > 0 {
        row.parallel_slots as u32
    } else {
        4
    };
    let result = crate::model_runtime::respawn_model(
        pool,
        crate::model_runtime::LoadOptions {
            library_id: lib.id.clone(),
            port: row.port as u16,
            context_size: Some(ctx),
            parallel: Some(parallel),
            agent_profile: false,
            mmproj_path: None, // auto-detect sibling mmproj on relaunch
        },
        &row.id,
    )
    .await
    .map_err(|e| format!("load_model: {e}"))?;
    tracing::info!(
        new_deployment = %result.deployment_id,
        pid = result.pid,
        port = result.port,
        "respawn complete"
    );
    Ok(true)
}

/// Outcome of [`evict_deployment_row`].
#[derive(Debug, Clone)]
pub struct EvictOutcome {
    /// Worker the evicted row belonged to.
    pub worker_name: String,
    /// Port the evicted row claimed.
    pub port: i32,
    /// True when the row was deleted immediately (it belonged to this node, so
    /// the systemd unit was stopped and any surviving listener killed first).
    /// False when the row belongs to another worker: it was only flipped to
    /// desired_state='retired' and that node's reconciler finishes the evict
    /// (Pass A kills a surviving process, Pass B removes the row).
    pub deleted: bool,
}

/// Evict a deployment row by UUID when `unload_model` can't — i.e. the row is
/// not in this node's deployment list, typically because the server process
/// died and this reconciler re-created the row (or the row lives under another
/// worker_name). Clears desired_state FIRST so the respawn loop stops, then
/// deletes the row (local) or leaves the delete to the owning node's reconciler
/// (remote). Without this fallback a dead-but-desired='active' endpoint could
/// only be stopped via the `--node`/`--port` form (observed on sia 2026-07-17:
/// the reconciler kept re-enabling a broken unit under fresh UUIDs while every
/// by-UUID unload bounced with "no deployment on this node").
pub async fn evict_deployment_row(
    pool: &sqlx::PgPool,
    deployment_id: &str,
) -> Result<EvictOutcome, String> {
    let uuid = sqlx::types::Uuid::parse_str(deployment_id)
        .map_err(|e| format!("bad deployment uuid '{deployment_id}': {e}"))?;

    // Fleet-wide lookup — deliberately NOT filtered by worker_name, unlike
    // unload_model's pg_list_deployments(Some(worker)) path that got us here.
    // Retire and fetch the row atomically. A separate SELECT followed by an
    // UPDATE leaves a window where the reconciler can still observe `active`
    // and respawn the dead deployment before eviction takes effect.
    let row = sqlx::query_as::<_, (String, i32, Option<i32>, Option<String>)>(
        "UPDATE fleet_model_deployments
            SET desired_state = 'retired'
          WHERE id = $1
      RETURNING worker_name, port, pid, library_id::text",
    )
    .bind(uuid)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("lookup deployment {deployment_id}: {e}"))?;
    let Some((worker_name, port, pid, library_id)) = row else {
        return Err(format!(
            "deployment '{deployment_id}' not found anywhere in fleet_model_deployments — \
             the reconciler may have re-created it under a new UUID; list current rows with \
             `ff model deployments`, or unload by endpoint: \
             `ff model unload --node <name> --port <port>`"
        ));
    };

    let this_node = crate::fleet_info::resolve_this_worker_name().await;
    if !evict_deletes_row(&worker_name, &this_node) {
        return Ok(EvictOutcome {
            worker_name,
            port,
            deleted: false,
        });
    }

    // Local row: finish what unload_model would have done. Stop the systemd
    // unit first so Restart=on-failure can't respawn the server, then reap
    // whatever still listens on the port (usually nothing — the process being
    // gone is why the by-UUID unload missed).
    #[cfg(target_os = "linux")]
    crate::model_runtime::stop_systemd_unit(port as u16).await;
    let _ = crate::model_runtime::stop_listener_on_port(port as u16, pid.map(|p| p as u32)).await;

    ff_db::pg_delete_deployment(pool, deployment_id)
        .await
        .map_err(|e| format!("pg_delete_deployment: {e}"))?;

    // Same library cool-down as unload_model: back to cold unless another
    // active deployment still serves this library row.
    if let Some(lid) = library_id {
        let _ = sqlx::query(
            "UPDATE fleet_model_library SET state = 'cold' WHERE id = $1::uuid \
             AND NOT EXISTS ( \
               SELECT 1 FROM fleet_model_deployments dep2 \
                WHERE dep2.library_id = $1::uuid \
                  AND dep2.desired_state = 'active' \
             )",
        )
        .bind(&lid)
        .execute(pool)
        .await;
    }
    Ok(EvictOutcome {
        worker_name,
        port,
        deleted: true,
    })
}

/// Whether the fleet-wide evict fallback may delete the row itself: only on the
/// owning node, where we can also stop the systemd unit / kill a survivor
/// first. Deleting a REMOTE row here would let that node's reconciler re-adopt
/// a still-running process as a fresh 'active' row; retiring it instead makes
/// that reconciler kill the process and drop the row. Case-insensitive to match
/// the `--node` comparison in the CLI. Pure so the decision is unit-testable.
fn evict_deletes_row(row_worker: &str, this_worker: &str) -> bool {
    row_worker.eq_ignore_ascii_case(this_worker)
}

/// Minimal deployment row for the reconciler — pulls just what we need plus
/// the new V90 `desired_state` column.
#[derive(Debug, Clone, sqlx::FromRow)]
struct DeploymentRow {
    id: String,
    port: i32,
    pid: Option<i32>,
    process_start_marker: Option<String>,
    library_id: Option<String>,
    catalog_id: Option<String>,
    desired_state: String,
    context_window: i32,
    /// V111 launched `--parallel`; 0 (via COALESCE) means "unknown" → respawn
    /// falls back to the historical default of 4.
    parallel_slots: i32,
}

async fn list_deployments_with_desired_state(
    pool: &sqlx::PgPool,
    worker_name: &str,
) -> Result<Vec<DeploymentRow>, String> {
    sqlx::query_as::<_, DeploymentRow>(
        "SELECT id::text AS id, port, pid, process_start_marker,
                library_id::text AS library_id,
                catalog_id,
                desired_state,
                COALESCE(context_window, 0) AS context_window,
                COALESCE(parallel_slots, 0) AS parallel_slots
         FROM fleet_model_deployments
         WHERE worker_name = $1",
    )
    .bind(worker_name)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("list deployments: {e}"))
}

/// Pick the best-matching library row for a running process's model path.
/// Returns (library_id, catalog_id) if we find one.
fn match_library_to_path(
    libs: &[ff_db::ModelLibraryRow],
    model_path: &str,
) -> (Option<String>, Option<String>) {
    if let Some(exact) = libs.iter().find(|r| r.file_path == model_path) {
        return (Some(exact.id.clone()), Some(exact.catalog_id.clone()));
    }
    // A deployment whose model path lives INSIDE a library directory matches
    // that library. Use component-wise `Path::starts_with` ONLY — a byte-wise
    // `str::starts_with` mis-attributes across models that merely share a string
    // prefix (e.g. a deployment under ".../qwen3-coder-30b" byte-starts-with a
    // ".../qwen3" library). Skip empty library paths, which `starts_with` would
    // otherwise treat as a prefix of every path.
    let path = Path::new(model_path);
    if let Some(by_prefix) = libs
        .iter()
        .filter(|r| !r.file_path.is_empty())
        .find(|r| path.starts_with(&r.file_path))
    {
        return (
            Some(by_prefix.id.clone()),
            Some(by_prefix.catalog_id.clone()),
        );
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autorestart_gate_accepts_only_explicit_enabled_values() {
        for enabled in ["on", "true", "1", " ON ", "TrUe"] {
            assert!(autorestart_enabled(Some(enabled)), "{enabled:?}");
        }
        for disabled in ["off", "false", "0", "", "yes", "enabled"] {
            assert!(!autorestart_enabled(Some(disabled)), "{disabled:?}");
        }
        assert!(!autorestart_enabled(None));
    }

    #[test]
    fn autorestart_health_requires_unhealthy_deployment() {
        assert!(health_requires_autorestart("unhealthy"));
        for status in ["healthy", "stale", "unknown", "loading", ""] {
            assert!(
                !health_requires_autorestart(status),
                "{status:?} must not trigger an automatic restart"
            );
        }
    }

    #[test]
    fn restart_spec_requires_library_and_valid_port() {
        assert_eq!(
            restart_spec(Some("library-id".to_string()), 55000),
            Some(("library-id".to_string(), 55000))
        );
        assert_eq!(
            restart_spec(Some("library-id".to_string()), 1),
            Some(("library-id".to_string(), 1))
        );
        assert_eq!(
            restart_spec(Some("library-id".to_string()), u16::MAX.into()),
            Some(("library-id".to_string(), u16::MAX))
        );
        assert_eq!(restart_spec(None, 55000), None);
        assert_eq!(restart_spec(Some("library-id".to_string()), 0), None);
        assert_eq!(restart_spec(Some("library-id".to_string()), -1), None);
        assert_eq!(
            restart_spec(Some("library-id".to_string()), i32::from(u16::MAX) + 1),
            None
        );
    }

    fn lib(id: &str, catalog_id: &str) -> ff_db::ModelLibraryRow {
        ff_db::ModelLibraryRow {
            id: id.to_string(),
            worker_name: "duncan".to_string(),
            catalog_id: catalog_id.to_string(),
            runtime: "llama.cpp".to_string(),
            quant: None,
            file_path: format!("/home/duncan/models/{catalog_id}"),
            size_bytes: 0,
            sha256: None,
            downloaded_at: chrono::Utc::now(),
            last_used_at: None,
            source_url: None,
            pinned: false,
        }
    }

    fn dead_row(catalog_id: Option<&str>) -> DeploymentRow {
        DeploymentRow {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            port: 55000,
            pid: None,
            process_start_marker: None,
            library_id: None,
            catalog_id: catalog_id.map(str::to_string),
            desired_state: "active".to_string(),
            context_window: 32768,
            parallel_slots: 1,
        }
    }

    #[test]
    fn recover_library_id_matches_by_catalog() {
        let libs = vec![lib("aaaa", "gemma4-31b-it"), lib("bbbb", "qwen36-35b-a3b")];
        // A dead row that still names its catalog model recovers the library_id
        // of the worker's matching library row — respawn instead of reap.
        let row = dead_row(Some("qwen36-35b-a3b"));
        assert_eq!(recover_library_id(&row, &libs), Some("bbbb".to_string()));
    }

    #[test]
    fn recover_library_id_none_without_catalog_or_match() {
        let libs = vec![lib("aaaa", "gemma4-31b-it")];
        // No catalog_id on the row → nothing to match on → reap.
        assert_eq!(recover_library_id(&dead_row(None), &libs), None);
        // catalog_id present but the worker has no library for it → reap.
        assert_eq!(
            recover_library_id(&dead_row(Some("qwen3-coder-30b")), &libs),
            None
        );
    }

    #[test]
    fn unrespawnable_only_when_library_id_missing() {
        // No library to load → permanently un-respawnable → reap.
        assert!(dead_active_is_unrespawnable(&None));
        // Has a library_id → respawn should be attempted (may still fail if the
        // library row is gone, but that path is allowed to retry).
        assert!(!dead_active_is_unrespawnable(&Some(
            "9d8d3fb8-e413-434d-af95-99a92bf55dba".to_string()
        )));
    }

    #[test]
    fn canonical_ports_cover_inference_slots_and_specials() {
        // llama.cpp / mlx slot window.
        assert!(port_is_canonical(CANONICAL_PORT_MIN));
        assert!(port_is_canonical(CANONICAL_PORT_MAX));
        assert!(port_is_canonical(55005));
        // vllm + ollama specials.
        assert!(port_is_canonical(51001));
        assert!(port_is_canonical(51003));
        assert!(port_is_canonical(11434));
        // Stray operator-launched ports are non-canonical.
        assert!(!port_is_canonical(8082));
        assert!(!port_is_canonical(CANONICAL_PORT_MAX + 1));
    }

    #[test]
    fn match_library_exact_path_wins() {
        let libs = vec![lib("id-a", "qwen3"), lib("id-b", "qwen3-coder-30b")];
        let (lib_id, cat) = match_library_to_path(&libs, "/home/duncan/models/qwen3-coder-30b");
        assert_eq!(lib_id.as_deref(), Some("id-b"));
        assert_eq!(cat.as_deref(), Some("qwen3-coder-30b"));
    }

    #[test]
    fn match_library_dir_prefix_matches_weights_inside() {
        // A deployment pointed at a file inside the library dir resolves to it.
        let libs = vec![lib("id-b", "qwen3-coder-30b")];
        let (lib_id, _) = match_library_to_path(
            &libs,
            "/home/duncan/models/qwen3-coder-30b/model-00001.safetensors",
        );
        assert_eq!(lib_id.as_deref(), Some("id-b"));
    }

    #[test]
    fn match_library_does_not_confuse_string_prefix_models() {
        // Regression: ".../qwen3-coder-30b/x" byte-starts-with the ".../qwen3"
        // library path, but they are different models. Component-wise matching
        // must resolve to qwen3-coder-30b, never qwen3 (listed first).
        let libs = vec![lib("id-a", "qwen3"), lib("id-b", "qwen3-coder-30b")];
        let (lib_id, cat) = match_library_to_path(
            &libs,
            "/home/duncan/models/qwen3-coder-30b/model.safetensors",
        );
        assert_eq!(lib_id.as_deref(), Some("id-b"));
        assert_eq!(cat.as_deref(), Some("qwen3-coder-30b"));
    }

    #[test]
    fn evict_deletes_row_only_on_owning_node() {
        // Owning node (case-insensitive, like the CLI --node comparison) may
        // delete the row after stopping the unit/listener.
        assert!(evict_deletes_row("sia", "sia"));
        assert!(evict_deletes_row("Sia", "sia"));
        // A remote row is only retired — its own reconciler completes the
        // evict, so a still-running process is never re-adopted as 'active'.
        assert!(!evict_deletes_row("sia", "duncan"));
    }

    #[test]
    fn match_library_empty_path_never_matches() {
        if std::env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && std::env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            return;
        }
        let mut l = lib("id-empty", "weird");
        l.file_path = String::new();
        let libs = vec![l];
        assert_eq!(
            match_library_to_path(&libs, "/home/duncan/models/anything"),
            (None, None)
        );
    }
}
