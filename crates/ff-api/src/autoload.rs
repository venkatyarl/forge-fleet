//! Auto-load: ensure a catalog model is deployed on the local node before routing.
//!
//! When the adaptive router selects a `catalog_id` that isn't currently deployed
//! on any healthy backend, [`ensure_deployed`] spawns an inference server for it
//! by shelling out to `ff model autoload <catalog_id>`. This keeps ff-api free
//! of any dependency cycle with ff-agent (which already depends on ff-api).
//!
//! The public API surface is intentionally small: [`ensure_deployed`] returns a
//! base URL (`http://host:port`) ready to be used as a `BackendEndpoint`.

use std::time::{Duration, Instant};

use ff_db::{pg_list_deployments, pg_list_library, pg_list_nodes};
use sqlx::PgPool;
use tracing::{info, warn};

/// If a deployment for `catalog_id` already exists on this node and is healthy,
/// return its URL. Otherwise, attempt to load it from `fleet_model_library` on
/// this node. Blocks up to 90s while the inference server warms up.
///
/// Returns a `http://host:port` URL on success.
pub async fn ensure_deployed(pool: &PgPool, catalog_id: &str) -> Result<String, String> {
    let node_name = resolve_this_node_name(pool).await;

    // 1. Already deployed and healthy?
    if let Some(url) = find_healthy_deployment(pool, &node_name, catalog_id).await? {
        return Ok(url);
    }

    // 2. Is there a library row for this catalog_id on this node?
    let libs = pg_list_library(pool, Some(&node_name))
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;
    if !libs.iter().any(|r| r.catalog_id == catalog_id) {
        return Err(format!(
            "model '{catalog_id}' not in library on '{node_name}'; \
             run `ff model download {catalog_id}` first"
        ));
    }

    // 3. Pick a free port (51001..=51020, skipping ones in deployments).
    let deps = pg_list_deployments(pool, Some(&node_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    let used_ports: std::collections::HashSet<i32> = deps.iter().map(|d| d.port).collect();
    if (51001i32..=51020).all(|p| used_ports.contains(&p)) {
        return Err("no free port in 51001..=51020 for autoload".to_string());
    }

    // 4. Shell out to `ff model autoload <catalog_id>`.
    //
    // We do this (instead of calling ff_agent::model_runtime::load_model directly)
    // because ff-agent already depends on ff-api, so pulling ff-agent in here
    // would form a dependency cycle. The `ff` binary is expected to be on PATH
    // or at $HOME/.local/bin/ff.
    info!(catalog_id, node = %node_name, "auto-loading model via `ff model autoload`");
    spawn_ff_autoload(catalog_id)?;

    // 5. Poll for a healthy deployment (up to 90s).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_status = "unknown".to_string();
    while Instant::now() < deadline {
        match find_healthy_deployment(pool, &node_name, catalog_id).await? {
            Some(url) => return Ok(url),
            None => {
                // Not yet healthy — record the latest status for diagnostics.
                if let Ok(deps) = pg_list_deployments(pool, Some(&node_name)).await {
                    if let Some(d) = deps
                        .iter()
                        .find(|d| d.catalog_id.as_deref() == Some(catalog_id))
                    {
                        last_status = d.health_status.clone();
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    warn!(catalog_id, last_status, "autoload timed out waiting for healthy deployment");
    Err(format!(
        "autoload of '{catalog_id}' did not become healthy within 90s (last_status: {last_status})"
    ))
}

// ─── helpers ──────────────────────────────────────────────────────────────

/// Look up a healthy deployment for (`node_name`, `catalog_id`) and return its
/// base URL if one is present.
async fn find_healthy_deployment(
    pool: &PgPool,
    node_name: &str,
    catalog_id: &str,
) -> Result<Option<String>, String> {
    let deps = pg_list_deployments(pool, Some(node_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    Ok(deps
        .into_iter()
        .find(|d| {
            d.catalog_id.as_deref() == Some(catalog_id) && d.health_status == "healthy"
        })
        .map(|d| format!("http://127.0.0.1:{}", d.port)))
}

/// Resolve the local node name. Mirrors `ff_agent::fleet_info::resolve_this_node_name`
/// but is duplicated here to avoid the ff-api → ff-agent dependency cycle.
async fn resolve_this_node_name(pool: &PgPool) -> String {
    if let Ok(v) = std::env::var("FORGEFLEET_NODE_NAME") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }

    let local_ips = local_ipv4_addrs();
    if let Ok(nodes) = pg_list_nodes(pool).await {
        for n in &nodes {
            if local_ips.contains(&n.ip) {
                return n.name.clone();
            }
            if let Some(alt) = n.alt_ips.as_array() {
                for v in alt {
                    if let Some(s) = v.as_str() {
                        if local_ips.contains(&s.to_string()) {
                            return n.name.clone();
                        }
                    }
                }
            }
        }
    }

    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().split('.').next().unwrap_or("unknown").to_lowercase())
        .unwrap_or_else(|| "unknown".into())
}

fn local_ipv4_addrs() -> Vec<String> {
    let out = std::process::Command::new("ifconfig").arg("-a").output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ips = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("inet ") {
            if let Some(ip) = rest.split_whitespace().next() {
                if !ip.starts_with("127.") {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    ips
}

/// Spawn `ff model autoload <catalog_id>` as a child process. We do NOT wait
/// for it to finish here — we poll the deployments table instead (see caller).
/// This lets us surface partial progress and a bounded timeout uniformly.
fn spawn_ff_autoload(catalog_id: &str) -> Result<(), String> {
    let bin = resolve_ff_binary();
    let child = std::process::Command::new(&bin)
        .args(["model", "autoload", catalog_id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {bin} model autoload {catalog_id}: {e}"))?;
    // Detach: we don't care about exit status; we poll the DB for progress.
    std::mem::forget(child);
    Ok(())
}

fn resolve_ff_binary() -> String {
    // Prefer $HOME/.local/bin/ff if present (standard install location),
    // otherwise fall back to "ff" on PATH.
    if let Ok(home) = std::env::var("HOME") {
        let candidate = std::path::PathBuf::from(&home).join(".local/bin/ff");
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    "ff".to_string()
}
