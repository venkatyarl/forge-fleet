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

    // ── Pass A — for each live process: adopt or refresh ──────────────────
    for proc_info in &procs {
        let Some(port) = proc_info.port else { continue };
        let port_i32 = port as i32;
        seen_ports.insert(port_i32);

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

            if let Err(e) = sqlx::query(
                "UPDATE fleet_model_deployments
                    SET health_status = $1,
                        last_health_at = NOW(),
                        pid = $2,
                        library_id = COALESCE(library_id, $3),
                        catalog_id = COALESCE(catalog_id, $4)
                  WHERE id = $5::uuid",
            )
            .bind(status)
            .bind(proc_info.pid as i32)
            .bind(lib_uuid)
            .bind(cat_id_str)
            .bind(&existing.id)
            .execute(pool)
            .await
            {
                tracing::warn!("failed to refresh deployment {}: {e}", existing.id);
            } else {
                summary.refreshed += 1;
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
                None,
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
                // Process died unexpectedly. Try to bring it back.
                match respawn_dead_deployment(pool, row, &libs).await {
                    Ok(true) => summary.respawned += 1,
                    Ok(false) => {} // unable, already logged
                    Err(e) => tracing::warn!(
                        "respawn deployment {} on port {}: {e}",
                        row.id,
                        row.port
                    ),
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

    // Delete the stale row first so load_model's upsert creates a fresh one
    // with the new pid. The row's desired_state was 'active' which carries
    // through the new row's default.
    let _ = ff_db::pg_delete_deployment(pool, &row.id).await;

    let ctx = if row.context_window > 0 {
        row.context_window as u32
    } else {
        32_768
    };
    let result = crate::model_runtime::load_model(
        pool,
        crate::model_runtime::LoadOptions {
            library_id: lib.id.clone(),
            port: row.port as u16,
            context_size: Some(ctx),
            parallel: Some(4),
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
                COALESCE(context_window, 0) AS context_window
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
