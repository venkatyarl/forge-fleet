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
    /// Stray processes for 'retired' deployments that were killed.
    pub killed: usize,
    /// Non-canonical port violations flipped to desired_state='retired' for
    /// removal on the same pass.
    pub port_violations: usize,
}

/// Run one reconcile pass. Returns counts for logging.
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
                "non-canonical port — killing process per canonical-port policy"
            );
            let _ = tokio::process::Command::new("kill")
                .args(["-TERM", &proc_info.pid.to_string()])
                .output()
                .await;
            summary.port_violations += 1;
            // If a deployment row was tracking this port, mark it retired
            // so the row gets cleaned up in pass B.
            if let Some(&existing) = db_by_port.get(&port_i32) {
                let _ = sqlx::query(
                    "UPDATE fleet_model_deployments
                        SET desired_state = 'retired'
                      WHERE id = $1::uuid AND desired_state = 'active'",
                )
                .bind(&existing.id)
                .execute(pool)
                .await;
            }
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
                // Operator wants this gone — kill the stray process. Row
                // will be deleted in pass B.
                let pid = proc_info.pid;
                tracing::info!(pid, port, "killing stray process for retired deployment");
                let _ = tokio::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output()
                    .await;
                summary.killed += 1;
                continue;
            }

            // desired_state='active': refresh + backfill library/catalog IDs
            // if missing (covers post-adopt library scan completing later).
            let needs_backfill = existing.library_id.is_none() || existing.catalog_id.is_none();
            let (lib_id_str, cat_id_str): (Option<String>, Option<String>) = if needs_backfill {
                if let Some(mp) = &proc_info.model_path {
                    match_library_to_path(&libs, mp)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
            let lib_uuid: Option<sqlx::types::Uuid> = lib_id_str
                .as_deref()
                .and_then(|s| sqlx::types::Uuid::parse_str(s).ok());

            // Self-heal context columns for adopted/under-probed deployments.
            // `context_window == 0` means we never recorded a real ctx (server
            // started out-of-band, or cmdline lacked --ctx-size). Probe the
            // live server for ground truth so the agent router — which filters
            // `usable_agent_ctx >= min_ctx` — can see this endpoint. Also
            // corrects a stale `parallel_slots` (e.g. veronica's DB said 2 but
            // /props reports 4). Only when healthy (an unhealthy server won't
            // answer /props anyway).
            let mut ctx_total: Option<i32> = None;
            let mut slots: Option<i32> = None;
            let mut usable: Option<i32> = None;
            if healthy && existing.context_window == 0 {
                if let Some((per_slot, total_slots)) =
                    crate::model_runtime::probe_agent_ctx(&proc_info.runtime, port).await
                {
                    ctx_total = Some(per_slot.saturating_mul(total_slots));
                    slots = Some(total_slots);
                    usable = Some(per_slot);
                } else if let (Some(cw), Some(ps)) =
                    (proc_info.context_window, proc_info.parallel_slots)
                {
                    // No HTTP probe for this runtime (mlx_lm.server exposes no
                    // ctx endpoint) — fall back to what the cmdline/model-config
                    // parse found. Without this, mlx rows kept usable_agent_ctx
                    // NULL forever and the V111 router never saw them.
                    ctx_total = Some(cw);
                    slots = Some(ps);
                    usable = Some(cw / ps.max(1));
                }
            }

            if let Err(e) = sqlx::query(
                "UPDATE fleet_model_deployments
                    SET health_status = $1,
                        last_health_at = NOW(),
                        pid = $2,
                        library_id = COALESCE(library_id, $3),
                        catalog_id = COALESCE(catalog_id, $4),
                        context_window = COALESCE($6::int, context_window),
                        parallel_slots = COALESCE($7::int, parallel_slots),
                        usable_agent_ctx = COALESCE($8::int, usable_agent_ctx)
                  WHERE id = $5::uuid",
            )
            .bind(status)
            .bind(proc_info.pid as i32)
            .bind(lib_uuid)
            .bind(cat_id_str)
            .bind(&existing.id)
            .bind(ctx_total)
            .bind(slots)
            .bind(usable)
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
            // ── Process exists, no DB row → adopt ─────────────────────────
            let (library_id, catalog_id) = if let Some(mp) = &proc_info.model_path {
                match_library_to_path(&libs, mp)
            } else {
                (None, None)
            };

            match ff_db::pg_upsert_deployment(
                pool,
                &worker_name,
                library_id.as_deref(),
                catalog_id.as_deref(),
                &proc_info.runtime,
                port_i32,
                Some(proc_info.pid as i32),
                status,
                // Adopt the real ctx + slot count parsed from the cmdline so an
                // out-of-band server still gets usable_agent_ctx recorded.
                proc_info.context_window,
                proc_info.parallel_slots,
            )
            .await
            {
                Ok(_) => summary.adopted += 1,
                Err(e) => tracing::warn!("adopt port {port}: {e}"),
            }
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
                // A dead `active` row with no library_id can NEVER be respawned
                // (respawn_dead_deployment needs a library to load), so retrying
                // it every 60s only spams the log and leaves a phantom 'unhealthy'
                // endpoint in `ff model deployments` (and the router's candidate
                // set) forever. Such rows come from adopting an out-of-band
                // process whose model path matched no library row; once that
                // process is gone the row is an unrecoverable zombie. Reap it.
                // An operator-loaded deployment always carries a library_id, so
                // this never discards real `ff model load` intent.
                if dead_active_is_unrespawnable(&row.library_id) {
                    tracing::info!(
                        port = row.port,
                        deployment = %row.id,
                        "reaping dead un-respawnable deployment (active, no library_id)"
                    );
                    if let Err(e) = ff_db::pg_delete_deployment(pool, &row.id).await {
                        tracing::warn!("delete un-respawnable deployment {}: {e}", row.id);
                    } else {
                        summary.removed += 1;
                    }
                    continue;
                }
                // Process died unexpectedly. Try to bring it back.
                match respawn_dead_deployment(pool, row, &libs).await {
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

/// Whether a dead `active` deployment row should be reaped instead of having a
/// respawn attempted. A respawn loads `row.library_id`, so a row with no
/// library_id can never come back — it's a zombie left by adopting an
/// out-of-band process whose model path matched no library. Pure predicate so
/// the Pass-B decision is unit-testable without a DB.
fn dead_active_is_unrespawnable(library_id: &Option<String>) -> bool {
    library_id.is_none()
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

    // NO delete-first. load_model upserts ON CONFLICT(worker_name, port), so it
    // REPLACES this row in place with the fresh pid. Deleting first was the
    // durability bug: if load_model then failed (e.g. RAM pressure during a
    // co-located build), the row was gone forever and the endpoint silently
    // vanished with no retry. Leaving the row intact (desired_state='active')
    // means a failed respawn is simply retried on the next 60s tick.
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
    let result = crate::model_runtime::load_model(
        pool,
        crate::model_runtime::LoadOptions {
            library_id: lib.id.clone(),
            port: row.port as u16,
            context_size: Some(ctx),
            parallel: Some(parallel),
            agent_profile: false,
            mmproj_path: None, // auto-detect sibling mmproj on relaunch
        },
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

/// Minimal deployment row for the reconciler — pulls just what we need plus
/// the new V90 `desired_state` column.
#[derive(Debug, sqlx::FromRow)]
struct DeploymentRow {
    id: String,
    port: i32,
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
        "SELECT id::text AS id, port,
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
    let path = Path::new(model_path);
    if let Some(by_prefix) = libs
        .iter()
        .find(|r| path.starts_with(&r.file_path) || model_path.starts_with(&r.file_path))
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
}
