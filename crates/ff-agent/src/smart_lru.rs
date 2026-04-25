//! Smart LRU eviction for the model library.
//!
//! When a node is over its disk quota, `plan_eviction` picks which library
//! rows to remove — but unlike a naive LRU it weighs several factors to
//! avoid thrashing:
//!
//!   - **Never** evict a library row referenced by an active deployment
//!   - **Never** evict a row used within the last `min_cold_days` (default 7)
//!   - **Prefer** evicting when another fleet node has the same
//!     (catalog_id, runtime) pair — cheap re-acquisition via LAN rsync
//!   - **Prefer** evicting redundant runtime variants for the node's own
//!     runtime — e.g. a GGUF sitting on an MLX-only node
//!   - **Weight** candidates by size (bigger = higher score for eviction)
//!   - **Weight** candidates by age (older last_used_at = higher score)
//!   - **Never** evict a row tagged as pinned (future: `pinned` column)
//!
//! The module returns a plan (ordered list of library ids) but does NOT
//! delete anything — callers are expected to run the eviction through the
//! normal delete path, which will refuse to remove models in use.

use std::collections::HashMap;

/// Knobs that control the eviction plan.
#[derive(Debug, Clone)]
pub struct LruPolicy {
    /// Minimum days since last_used_at before a row can be considered cold.
    pub min_cold_days: i64,
    /// Target free space fraction after eviction (0.0–1.0). Default 0.3 (30%).
    pub target_free_frac: f64,
    /// Only consider evicting from this single node (`None` means any).
    pub node_filter: Option<String>,
}

impl Default for LruPolicy {
    fn default() -> Self {
        Self {
            min_cold_days: 7,
            target_free_frac: 0.3,
            node_filter: None,
        }
    }
}

/// One eviction candidate, with reasoning scores for debugging.
#[derive(Debug, Clone)]
pub struct EvictionCandidate {
    pub library_id: String,
    pub node_name: String,
    pub catalog_id: String,
    pub runtime: String,
    pub file_path: String,
    pub size_bytes: u64,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// A concrete plan: candidates ordered by eviction priority (highest first),
/// plus the projected bytes freed if the plan is executed in full.
#[derive(Debug, Clone, Default)]
pub struct EvictionPlan {
    pub candidates: Vec<EvictionCandidate>,
    pub total_bytes_freed: u64,
}

/// Build an eviction plan for a node that's over quota.
///
/// Uses the latest `fleet_disk_usage` row to decide how many bytes we need
/// to free. Returns an empty plan if the node isn't actually over quota.
pub async fn plan_eviction(
    pool: &sqlx::PgPool,
    node_name: &str,
    policy: &LruPolicy,
) -> Result<EvictionPlan, String> {
    // Fetch latest disk usage row for this node.
    let usage = ff_db::pg_latest_disk_usage(pool)
        .await
        .map_err(|e| format!("pg_latest_disk_usage: {e}"))?;
    let node_usage = usage.iter().find(|(n, ..)| n == node_name);
    let (total, used, _free) = match node_usage {
        Some((_, _, total, used, free, _, _)) => (*total as u64, *used as u64, *free as u64),
        None => return Ok(EvictionPlan::default()), // no disk sample yet
    };
    let node = ff_db::pg_get_node(pool, node_name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
        .ok_or_else(|| format!("node '{node_name}' not in fleet_nodes"))?;
    let quota_bytes = (total as f64 * node.disk_quota_pct as f64 / 100.0) as u64;

    // Nothing to do if not over quota.
    if used <= quota_bytes {
        return Ok(EvictionPlan::default());
    }
    let target_free = (total as f64 * policy.target_free_frac) as u64;
    let needed_free = target_free.saturating_sub(total.saturating_sub(used));
    let mut to_free = (used - quota_bytes).max(needed_free);

    // Fleet-wide library snapshot — needed for cross-node duplicate detection.
    let all_lib = ff_db::pg_list_library(pool, None)
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;
    // Map (catalog_id, runtime) → list of (node, lib_id)
    let mut peer_copies: HashMap<(String, String), Vec<String>> = HashMap::new();
    for row in &all_lib {
        peer_copies
            .entry((row.catalog_id.clone(), row.runtime.clone()))
            .or_default()
            .push(row.node_name.clone());
    }
    // Keep per-node library (the candidate pool).
    let node_lib: Vec<&ff_db::ModelLibraryRow> = all_lib
        .iter()
        .filter(|r| r.node_name == node_name)
        .collect();

    // Deployments on this node — rows referenced here are off-limits.
    let deployments = ff_db::pg_list_deployments(pool, Some(node_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    let locked_ids: std::collections::HashSet<String> = deployments
        .iter()
        .filter_map(|d| d.library_id.clone())
        .collect();

    let now = chrono::Utc::now();
    let mut candidates: Vec<EvictionCandidate> = Vec::new();

    for row in &node_lib {
        if locked_ids.contains(&row.id) {
            continue; // active deployment
        }

        // Days since last_used_at (or downloaded_at if never used).
        let ref_time = row.last_used_at.unwrap_or(row.downloaded_at);
        let age_days = (now - ref_time).num_days().max(0);
        if age_days < policy.min_cold_days {
            continue;
        }

        // Score: bigger + older = more evictable.
        // Base score grows with size (GiB) * age (days).
        let gb = row.size_bytes as f64 / (1u64 << 30) as f64;
        let mut score = gb * age_days as f64;
        let mut reasons: Vec<String> =
            vec![format!("size={:.1}GiB", gb), format!("age={}d", age_days)];

        // Peer-copy bonus: if another node has this (catalog_id, runtime), eviction is cheap.
        let key = (row.catalog_id.clone(), row.runtime.clone());
        if let Some(peers) = peer_copies.get(&key) {
            let others: Vec<&String> = peers.iter().filter(|n| *n != node_name).collect();
            if !others.is_empty() {
                score *= 1.5;
                reasons.push(format!("peer-copies={}", others.len()));
            }
        }

        // Wrong-runtime bonus: this row's runtime doesn't match node's runtime.
        if row.runtime != node.runtime && node.runtime != "unknown" {
            score *= 1.4;
            reasons.push(format!("wrong-runtime({}/{})", row.runtime, node.runtime));
        }

        candidates.push(EvictionCandidate {
            library_id: row.id.clone(),
            node_name: row.node_name.clone(),
            catalog_id: row.catalog_id.clone(),
            runtime: row.runtime.clone(),
            file_path: row.file_path.clone(),
            size_bytes: row.size_bytes as u64,
            score,
            reasons,
        });
    }

    // Sort by score desc.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Trim to just enough to free the target bytes.
    let mut freed: u64 = 0;
    let mut chosen: Vec<EvictionCandidate> = Vec::new();
    for c in candidates {
        if to_free == 0 {
            break;
        }
        let size = c.size_bytes;
        chosen.push(c);
        freed = freed.saturating_add(size);
        to_free = to_free.saturating_sub(size);
    }

    Ok(EvictionPlan {
        candidates: chosen,
        total_bytes_freed: freed,
    })
}
