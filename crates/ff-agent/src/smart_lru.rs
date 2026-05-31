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
    pub worker_name: String,
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
    worker_name: &str,
    policy: &LruPolicy,
) -> Result<EvictionPlan, String> {
    // Fetch latest disk usage row for this node.
    let usage = ff_db::pg_latest_disk_usage(pool)
        .await
        .map_err(|e| format!("pg_latest_disk_usage: {e}"))?;
    let node_usage = usage.iter().find(|(n, ..)| n == worker_name);
    let (total, used, _free) = match node_usage {
        Some((_, _, total, used, free, _, _)) => (*total as u64, *used as u64, *free as u64),
        None => return Ok(EvictionPlan::default()), // no disk sample yet
    };
    let node = ff_db::pg_get_node(pool, worker_name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
        .ok_or_else(|| format!("node '{worker_name}' not in fleet_workers"))?;
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
            .push(row.worker_name.clone());
    }
    // Keep per-node library (the candidate pool).
    let node_lib: Vec<&ff_db::ModelLibraryRow> = all_lib
        .iter()
        .filter(|r| r.worker_name == worker_name)
        .collect();

    // Deployments on this node — rows referenced here are off-limits.
    let deployments = ff_db::pg_list_deployments(pool, Some(worker_name))
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
        if row.pinned {
            continue; // V118: pinned rows are never evicted.
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
            let others: Vec<&String> = peers.iter().filter(|n| *n != worker_name).collect();
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
            worker_name: row.worker_name.clone(),
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

// ═══════════════════════════════════════════════════════════════════════════
// V118: MOVE-vs-DELETE classified planning.
//
// The base `plan_eviction` decides WHICH rows to evict and in what order. The
// classifier below decides HOW to evict each one without losing the only copy
// of a still-wanted model:
//
//   DELETE when ANY of:
//     • wrong-runtime for this node (a GGUF on an mlx-only Mac, etc.) — it can
//       never run here, so removing it costs nothing locally.
//     • retired (catalog lifecycle_status='retired') — nobody wants it back.
//     • cold(>=min_cold_days) AND a peer copy exists elsewhere — cheap re-pull
//       over the LAN if we ever need it again.
//
//   MOVE when: none of the DELETE conditions hold — i.e. the model is STILL
//     WANTED (right-runtime, not retired) and removing it would lose the ONLY
//     copy. We relocate it to a target node that (a) can run its runtime and
//     (b) has free disk >= size + headroom. The SOURCE is deleted only AFTER
//     the move is actuated and verified (handled by the reconcile tick, not
//     here — this module only PLANS).
//
//   SKIP (action=None target): a MOVE is warranted but no eligible target node
//     exists. We never delete the only copy of a wanted model.
// ═══════════════════════════════════════════════════════════════════════════

/// What to do with one over-quota candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskAction {
    /// Remove the local copy outright (wrong-runtime / retired / peer-backed).
    Delete,
    /// Relocate to `target_node` first, then delete the source after verify.
    Move,
    /// A move is warranted but no eligible target was found — do nothing,
    /// surface for the operator. NEVER deletes the only copy.
    Skip,
}

impl DiskAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiskAction::Delete => "delete",
            DiskAction::Move => "move",
            DiskAction::Skip => "skip",
        }
    }
}

/// A classified over-quota candidate: the underlying eviction candidate plus the
/// move/delete decision and (for moves) the chosen target node.
#[derive(Debug, Clone)]
pub struct ClassifiedCandidate {
    pub library_id: String,
    pub worker_name: String,
    pub catalog_id: String,
    pub runtime: String,
    pub file_path: String,
    pub size_bytes: u64,
    pub action: DiskAction,
    /// Set only when `action == Move`: where the model relocates to.
    pub target_node: Option<String>,
    pub reasons: Vec<String>,
}

/// A classified plan for one over-quota node.
#[derive(Debug, Clone, Default)]
pub struct ClassifiedPlan {
    pub worker_name: String,
    pub candidates: Vec<ClassifiedCandidate>,
    pub total_bytes_freed: u64,
}

/// Headroom multiplier + floor a target must satisfy to accept a MOVE. Mirrors
/// the transfer pre-check (`free >= size * 1.1 + floor`) so the planner and the
/// actuator agree on what "fits".
const MOVE_HEADROOM_FACTOR: f64 = 1.1;
const MOVE_HEADROOM_FLOOR_BYTES: u64 = 5 * (1u64 << 30); // 5 GiB

/// Build a MOVE-vs-DELETE classified plan for an over-quota node.
///
/// Runs the base `plan_eviction` to get the ordered candidate set (which already
/// honors locked/cold/pinned), then classifies each candidate. Pure decision
/// logic — actuates nothing.
pub async fn plan_classified_eviction(
    pool: &sqlx::PgPool,
    worker_name: &str,
    policy: &LruPolicy,
) -> Result<ClassifiedPlan, String> {
    let base = plan_eviction(pool, worker_name, policy).await?;
    if base.candidates.is_empty() {
        return Ok(ClassifiedPlan {
            worker_name: worker_name.to_string(),
            ..Default::default()
        });
    }

    let node = ff_db::pg_get_node(pool, worker_name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
        .ok_or_else(|| format!("node '{worker_name}' not in fleet_workers"))?;

    // Fleet-wide library → peer-copy map keyed (catalog_id, runtime).
    let all_lib = ff_db::pg_list_library(pool, None)
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;
    let mut peer_nodes: HashMap<(String, String), Vec<String>> = HashMap::new();
    for row in &all_lib {
        peer_nodes
            .entry((row.catalog_id.clone(), row.runtime.clone()))
            .or_default()
            .push(row.worker_name.clone());
    }

    // Retired catalog ids — these are DELETE even as the only copy.
    let retired = ff_db::pg_retired_catalog_ids(pool)
        .await
        .map_err(|e| format!("pg_retired_catalog_ids: {e}"))?;

    // Placement candidates for MOVE target selection (online + runtime + RAM).
    let placement = ff_db::pg_placement_candidates(pool)
        .await
        .map_err(|e| format!("pg_placement_candidates: {e}"))?;

    let mut out: Vec<ClassifiedCandidate> = Vec::new();
    let mut freed: u64 = 0;

    for c in &base.candidates {
        let wrong_runtime = c.runtime != node.runtime && node.runtime != "unknown";
        let is_retired = retired.contains(&c.catalog_id);
        let peers: Vec<&String> = peer_nodes
            .get(&(c.catalog_id.clone(), c.runtime.clone()))
            .map(|v| v.iter().filter(|n| *n != worker_name).collect())
            .unwrap_or_default();
        let has_peer = !peers.is_empty();

        let mut reasons = c.reasons.clone();

        let (action, target) = if wrong_runtime {
            reasons.push("DELETE: wrong-runtime for this node".into());
            (DiskAction::Delete, None)
        } else if is_retired {
            reasons.push("DELETE: catalog retired".into());
            (DiskAction::Delete, None)
        } else if has_peer {
            reasons.push(format!("DELETE: {} peer copy/copies exist", peers.len()));
            (DiskAction::Delete, None)
        } else {
            // Still wanted, only copy → MOVE if we can find a home.
            match pick_move_target(&placement, worker_name, &c.runtime, c.size_bytes, pool).await? {
                Some(t) => {
                    reasons.push(format!("MOVE: only copy, still wanted → {t}"));
                    (DiskAction::Move, Some(t))
                }
                None => {
                    reasons.push("SKIP: only copy, no eligible target with free space".into());
                    (DiskAction::Skip, None)
                }
            }
        };

        if matches!(action, DiskAction::Delete | DiskAction::Move) {
            freed = freed.saturating_add(c.size_bytes);
        }

        out.push(ClassifiedCandidate {
            library_id: c.library_id.clone(),
            worker_name: c.worker_name.clone(),
            catalog_id: c.catalog_id.clone(),
            runtime: c.runtime.clone(),
            file_path: c.file_path.clone(),
            size_bytes: c.size_bytes,
            action,
            target_node: target,
            reasons,
        });
    }

    Ok(ClassifiedPlan {
        worker_name: worker_name.to_string(),
        candidates: out,
        total_bytes_freed: freed,
    })
}

/// Pick a MOVE target: an online, runtime-compatible node (other than the source)
/// whose latest disk sample shows free >= size*1.1 + floor. Deterministic: among
/// eligible targets, prefer the one with the MOST free bytes (best long-term fit),
/// breaking ties by worker_name for stable output.
async fn pick_move_target(
    placement: &[ff_db::PlacementCandidate],
    source_node: &str,
    runtime: &str,
    size_bytes: u64,
    pool: &sqlx::PgPool,
) -> Result<Option<String>, String> {
    let need = (size_bytes as f64 * MOVE_HEADROOM_FACTOR) as u64 + MOVE_HEADROOM_FLOOR_BYTES;

    let mut best: Option<(u64, String)> = None;
    for host in placement {
        if host.worker_name == source_node {
            continue;
        }
        if host.status != "online" {
            continue;
        }
        if !target_runtime_compatible(host, runtime) {
            continue;
        }
        // Free disk from the latest sample (the V118 resource read).
        let Some((free, _total, _age)) = ff_db::pg_node_free_disk(pool, &host.worker_name)
            .await
            .map_err(|e| format!("pg_node_free_disk({}): {e}", host.worker_name))?
        else {
            continue; // never sampled → can't prove it fits.
        };
        let free = free.max(0) as u64;
        if free < need {
            continue;
        }
        match &best {
            Some((bf, bn)) if (free, &host.worker_name) <= (*bf, bn) => {}
            _ => best = Some((free, host.worker_name.clone())),
        }
    }
    Ok(best.map(|(_, n)| n))
}

/// Whether `host` can RUN a model launched with `runtime`. Same rule as the
/// autoscaler's `runtime_compatible`, restated here so smart_lru doesn't depend
/// on the autoscaler module: mlx ⇒ macOS only; vllm ⇒ CUDA/GB10; llama.cpp ⇒ any.
fn target_runtime_compatible(host: &ff_db::PlacementCandidate, runtime: &str) -> bool {
    let gpu = host.gpu_kind.as_deref().unwrap_or("none");
    match runtime {
        "mlx" => host.os_family == "macos",
        "vllm" => matches!(gpu, "nvidia_cuda" | "gb10"),
        _ => true,
    }
}
