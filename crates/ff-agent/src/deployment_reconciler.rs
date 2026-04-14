//! Deployment reconciler — keep `fleet_model_deployments` in sync with reality.
//!
//! Scans local processes via [`crate::model_runtime::list_local_processes`] and
//! compares with what the DB believes is deployed on this node. Any drift is
//! healed:
//!   - Process running, no DB row        → insert a deployment (with health-check)
//!   - DB row present, no process        → delete the deployment row
//!   - Both present                      → refresh last_health_at + health_status
//!
//! Intended to be called periodically (every 60–120s) by a long-running daemon.

use std::collections::HashMap;
use std::path::Path;

/// Summary of a reconcile pass.
#[derive(Debug, Clone, Default)]
pub struct ReconcileSummary {
    /// Existing processes that were newly inserted into the DB.
    pub adopted: usize,
    /// DB rows removed because the process was gone.
    pub removed: usize,
    /// Existing rows whose health_status was refreshed.
    pub refreshed: usize,
}

/// Run one reconcile pass. Returns counts for logging.
pub async fn reconcile_local(pool: &sqlx::PgPool) -> Result<ReconcileSummary, String> {
    let node_name = crate::fleet_info::resolve_this_node_name().await;

    // 1. Snapshot what's actually running on this host.
    let procs = crate::model_runtime::list_local_processes().await;

    // 2. Snapshot what the DB thinks is deployed on this host.
    let db_rows = ff_db::pg_list_deployments(pool, Some(&node_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;

    // Index DB rows by port for quick lookup.
    let db_by_port: HashMap<i32, &ff_db::ModelDeploymentRow> =
        db_rows.iter().map(|r| (r.port, r)).collect();

    let libs = ff_db::pg_list_library(pool, Some(&node_name))
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;

    let mut summary = ReconcileSummary::default();
    let mut seen_ports: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for proc_info in &procs {
        let Some(port) = proc_info.port else { continue };
        let port_i32 = port as i32;
        seen_ports.insert(port_i32);

        // Health check before writing status.
        let healthy = crate::model_runtime::probe_health_public(
            &proc_info.runtime,
            port,
            std::time::Duration::from_secs(3),
        )
        .await;
        let status = if healthy { "healthy" } else { "unhealthy" };

        if let Some(&existing) = db_by_port.get(&port_i32) {
            // Refresh existing row.
            if let Err(e) = sqlx::query(
                "UPDATE fleet_model_deployments
                    SET health_status = $1, last_health_at = NOW(), pid = $2
                  WHERE id = $3::uuid",
            )
            .bind(status)
            .bind(proc_info.pid as i32)
            .bind(&existing.id)
            .execute(pool)
            .await
            {
                tracing::warn!("failed to refresh deployment {}: {e}", existing.id);
            } else {
                summary.refreshed += 1;
            }
        } else {
            // Adopt: find the library row whose file_path matches the running model.
            let (library_id, catalog_id) = if let Some(mp) = &proc_info.model_path {
                match_library_to_path(&libs, mp)
            } else {
                (None, None)
            };

            match ff_db::pg_upsert_deployment(
                pool,
                &node_name,
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

    // 3. Anything in DB but not in process list → remove.
    for row in &db_rows {
        if !seen_ports.contains(&row.port) {
            if let Err(e) = ff_db::pg_delete_deployment(pool, &row.id).await {
                tracing::warn!("delete stale deployment {}: {e}", row.id);
            } else {
                summary.removed += 1;
            }
        }
    }

    Ok(summary)
}

/// Pick the best-matching library row for a running process's model path.
/// Returns (library_id, catalog_id) if we find one.
fn match_library_to_path(
    libs: &[ff_db::ModelLibraryRow],
    model_path: &str,
) -> (Option<String>, Option<String>) {
    // Prefer exact match; fall back to prefix/contains.
    if let Some(exact) = libs.iter().find(|r| r.file_path == model_path) {
        return (Some(exact.id.clone()), Some(exact.catalog_id.clone()));
    }
    let path = Path::new(model_path);
    // Try matching by parent dir or filename.
    if let Some(by_prefix) = libs.iter().find(|r| {
        path.starts_with(&r.file_path) || model_path.starts_with(&r.file_path)
    }) {
        return (Some(by_prefix.id.clone()), Some(by_prefix.catalog_id.clone()));
    }
    (None, None)
}
