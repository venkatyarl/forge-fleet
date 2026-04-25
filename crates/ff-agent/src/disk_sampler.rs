//! Disk usage sampler — snapshots `<models_dir>` filesystem usage to Postgres.
//!
//! Called on a schedule by a long-running daemon or manually via `ff model disk --sample`.
//! Writes rows into `fleet_disk_usage` for historical tracking and quota alerting.

use std::path::PathBuf;

/// One disk-usage snapshot for the current node.
#[derive(Debug, Clone)]
pub struct DiskSample {
    pub node_name: String,
    pub models_dir: PathBuf,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub models_bytes: u64,
    pub quota_pct: u32,
    pub over_quota: bool,
}

/// Sample the current host's disk usage and insert a row into `fleet_disk_usage`.
/// Returns the sample (also for display / alerting).
pub async fn sample_local_disk(pool: &sqlx::PgPool) -> Result<DiskSample, String> {
    let node_name = crate::fleet_info::resolve_this_node_name().await;

    // Look up node row for models_dir + quota.
    let node = ff_db::pg_get_node(pool, &node_name)
        .await
        .map_err(|e| format!("pg_get_node({node_name}): {e}"))?
        .ok_or_else(|| format!("node '{node_name}' not in fleet_nodes"))?;

    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let models_dir = expand_tilde(&node.models_dir, &home);
    let quota_pct = node.disk_quota_pct.max(1) as u32;

    // Get FS totals via `df -Pk <models_dir>` — POSIX 1024-byte blocks, portable across Mac/Linux.
    let df_out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(&models_dir)
        .output()
        .map_err(|e| format!("df spawn: {e}"))?;
    if !df_out.status.success() {
        return Err(format!(
            "df failed: {}",
            String::from_utf8_lossy(&df_out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&df_out.stdout);
    let last_line = text.lines().last().unwrap_or("").trim();
    // POSIX columns: Filesystem 1024-blocks Used Available Capacity Mountpoint
    let parts: Vec<&str> = last_line.split_whitespace().collect();
    if parts.len() < 6 {
        return Err(format!("unexpected df output: {last_line}"));
    }
    let total_k: u64 = parts[1].parse().unwrap_or(0);
    let used_k: u64 = parts[2].parse().unwrap_or(0);
    let free_k: u64 = parts[3].parse().unwrap_or(0);
    let total_bytes = total_k.saturating_mul(1024);
    let used_bytes = used_k.saturating_mul(1024);
    let free_bytes = free_k.saturating_mul(1024);

    // Recursively sum file sizes inside models_dir.
    let models_bytes = if models_dir.is_dir() {
        dir_size(&models_dir)
    } else {
        0
    };

    // Over quota check against configured percentage of total disk.
    let used_pct_x100 = if total_bytes == 0 {
        0
    } else {
        (used_bytes.saturating_mul(100) / total_bytes) as u32
    };
    let over_quota = used_pct_x100 > quota_pct;

    // Persist.
    ff_db::pg_insert_disk_usage(
        pool,
        &node_name,
        &models_dir.to_string_lossy(),
        total_bytes as i64,
        used_bytes as i64,
        free_bytes as i64,
        models_bytes as i64,
    )
    .await
    .map_err(|e| format!("pg_insert_disk_usage: {e}"))?;

    // If we crossed the quota line, enqueue a deferred task so operators notice.
    // Idempotent: only enqueue if no identical alert is already pending/dispatchable.
    if over_quota {
        let _ = maybe_alert_over_quota(pool, &node_name, used_bytes, total_bytes, quota_pct).await;
    }

    Ok(DiskSample {
        node_name,
        models_dir,
        total_bytes,
        used_bytes,
        free_bytes,
        models_bytes,
        quota_pct,
        over_quota,
    })
}

/// Enqueue a `manual` deferred task flagging disk-quota breach for operator
/// review. No-op if one is already pending for this node.
async fn maybe_alert_over_quota(
    pool: &sqlx::PgPool,
    node_name: &str,
    used_bytes: u64,
    total_bytes: u64,
    quota_pct: u32,
) -> Result<(), String> {
    // De-dupe: see if an alert for this node is already open.
    let rows = ff_db::pg_list_deferred(pool, Some("pending"), 50)
        .await
        .map_err(|e| format!("pg_list_deferred: {e}"))?;
    let already_alerted = rows
        .iter()
        .any(|r| r.title.starts_with("⚠ disk quota exceeded on ") && r.title.contains(node_name));
    if already_alerted {
        return Ok(());
    }

    let used_pct = if total_bytes == 0 {
        0
    } else {
        used_bytes * 100 / total_bytes
    };
    let title = format!("⚠ disk quota exceeded on {node_name} ({}%)", used_pct);
    let payload = serde_json::json!({
        "note": format!(
            "Disk usage {}% exceeds quota {}% on {}. \
             Review with: ff model prune --node {} \
             Delete candidates with: ff model delete <library-id> --yes",
            used_pct, quota_pct, node_name, node_name
        ),
    });
    let _ = ff_db::pg_enqueue_deferred(
        pool,
        &title,
        "manual",
        &payload,
        "manual", // trigger_type: user must act
        &serde_json::json!({}),
        Some(node_name),
        &serde_json::json!([]),
        Some("disk-sampler"),
        Some(1), // max_attempts — this is informational
    )
    .await
    .map_err(|e| format!("pg_enqueue_deferred: {e}"))?;
    Ok(())
}

/// Recursively sum the size of every regular file under `root`. Best-effort — silently
/// skips unreadable entries (permission errors, symlinks to missing targets).
fn dir_size(root: &std::path::Path) -> u64 {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut total: u64 = 0;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_file() {
                if let Ok(meta) = e.metadata() {
                    total = total.saturating_add(meta.len());
                }
            } else if ft.is_dir() {
                stack.push(e.path());
            }
            // symlinks: skip to avoid cycles
        }
    }
    total
}

fn expand_tilde(p: &str, home: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else if p == "~" {
        PathBuf::from(home)
    } else {
        PathBuf::from(p)
    }
}
