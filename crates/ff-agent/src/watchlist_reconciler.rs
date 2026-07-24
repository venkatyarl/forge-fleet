//! Autopilot-5: auto-download watchlist.
//!
//! Catalog rows with `fleet_model_catalog.watchlist = TRUE` (V252) are models
//! the fleet wants on disk without operator action. Each pass (leader-gated by
//! the caller, like the model auto-upgrade tick) runs two phases:
//!
//! **Phase 1 — download.** Watchlisted models with no `fleet_model_library`
//! presence anywhere in the fleet are fetched: pick the online node with the
//! most combined free disk + free RAM (live heartbeat occupancy, not the
//! machine's total) that fits the variant's estimated size plus
//! [`BUILD_RESERVE_GB`], and enqueue a deferred `ff model download` on that
//! node. The download path reuses the existing `hf_download` streaming
//! (resume + progress + placement guard), and the post-download library
//! re-scan registers the row with the schema-default `state = 'cold'`.
//!
//! **Phase 2 — challenger registration (Autopilot-4 integration).** A
//! watchlisted model that HAS a cold library copy but no live deployment is
//! registered as a bandit challenger when a same-workload serving incumbent
//! exists: its catalog `tier` is aligned to the incumbent's tier (tier + 0 —
//! same tier, never above), and a deferred `ff model load` starts it on the
//! node holding the copy. Same tier + a healthy deployment is exactly what
//! makes it an arm for Autopilot-4: `pg_route_deployments` epsilon-greedy
//! explores same-tier peers, and the daily tier reconcile groups
//! (tier, workload) peers as incumbent vs challengers by build volume.
//! The phase detects whether Autopilot-4 has landed at runtime by probing for
//! its schema artifact (the `v_model_utilization` view) — version-ledger
//! numbers are unreliable here (V246 collided across branches) — and holds
//! registration until the view exists, re-checking every tick.
//!
//! Safety rails, in order:
//!   - `gated = TRUE` catalog rows are never auto-downloaded, flag or not.
//!   - Licenses outside the model-scout allowlist — INCLUDING absent/unknown
//!     licenses — are skipped (fail closed, same conservatism as scout
//!     promotion), as are unknown-size variants.
//!   - A pending/running `watchlist-download:` / `watchlist-challenger:`
//!     deferred task for the model suppresses re-enqueue, so a slow
//!     multi-hour pull or load isn't duplicated by the next tick.

use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};
use tracing::{info, warn};

/// Disk headroom (GB) kept free on the chosen node beyond the model's
/// estimated size, so a watchlist pull never starves co-located builds /
/// worktrees of scratch space.
pub const BUILD_RESERVE_GB: f64 = 30.0;

/// Cap on downloads enqueued per pass (mirrors the auto-upgrade tick's
/// bounded dispatch; the next tick picks up the remainder).
const MAX_DOWNLOADS_PER_PASS: i64 = 3;

/// Port range scanned for a free inference slot when starting a challenger.
/// 55000 is the canonical llama.cpp/mlx slot (`ff model load` default);
/// 51001/51003 are vllm's and are deliberately outside this range.
const CHALLENGER_PORT_RANGE: std::ops::RangeInclusive<u16> = 55000..=55019;

/// What one reconcile pass did.
#[derive(Debug, Default)]
pub struct WatchlistSummary {
    /// Watchlisted models missing from the library and not already queued.
    pub considered: usize,
    /// Downloads enqueued to the defer queue this pass.
    pub enqueued: usize,
    /// Candidates skipped (license, unknown size, or no node fits).
    pub skipped: usize,
    /// Cold watchlist arrivals registered as bandit challengers (tier aligned
    /// to the incumbent's + deferred `ff model load` enqueued).
    pub challengers_registered: usize,
    /// Cold arrivals left unregistered this pass (Autopilot-4 layer not
    /// present yet, no same-workload serving incumbent, or no free port).
    pub challengers_waiting: usize,
}

impl WatchlistSummary {
    /// True when the pass did and found nothing worth logging.
    pub fn is_noop(&self) -> bool {
        self.considered == 0 && self.challengers_registered == 0 && self.challengers_waiting == 0
    }
}

/// A node eligible to receive a watchlist download.
#[derive(Debug, Clone)]
pub struct NodeCapacity {
    pub name: String,
    pub runtime: String,
    pub free_disk_gb: f64,
    /// RAM currently unused on the node (capacity minus the latest heartbeat
    /// `ram_used_gb` sample) — NOT the machine's total. Placement must judge
    /// whether the model could be loaded *now*, on the node as it is running,
    /// not on an idealized empty box.
    pub free_ram_gb: f64,
}

/// The placement one pass chose for a model.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedDownload {
    pub node: String,
    pub runtime: String,
    pub size_gb: f64,
}

/// A same-workload catalog model currently serving (healthy deployment),
/// with recent build volume from Autopilot-4's reward view — the incumbent
/// pick applies the same "most-used peer is the incumbent" rule as the
/// daily tier reconcile.
#[derive(Debug, Clone, PartialEq)]
pub struct IncumbentCandidate {
    pub catalog_id: String,
    pub tier: i32,
    pub builds: i64,
}

/// Pick the incumbent among same-workload serving models: most recent builds
/// wins, ties break toward the lexicographically-lower catalog id for
/// determinism (mirrors the daily reconcile's ordering).
pub fn pick_incumbent(candidates: &[IncumbentCandidate]) -> Option<&IncumbentCandidate> {
    candidates.iter().max_by(|a, b| {
        a.builds
            .cmp(&b.builds)
            .then_with(|| b.catalog_id.cmp(&a.catalog_id))
    })
}

/// Challenger enters at the incumbent's tier + 0: same tier (so epsilon-greedy
/// routing and the daily tier reconcile treat the pair as arms of one group),
/// never above it. Returns the tier to write, or `None` when already aligned.
pub fn align_challenger_tier(challenger_tier: i32, incumbent_tier: i32) -> Option<i32> {
    (challenger_tier != incumbent_tier).then_some(incumbent_tier)
}

/// First free port in [`CHALLENGER_PORT_RANGE`] given the node's ports already
/// held by deployment rows (any health state — rows are freed on unload).
pub fn pick_challenger_port(used: &[i32]) -> Option<u16> {
    CHALLENGER_PORT_RANGE
        .into_iter()
        .find(|p| !used.contains(&(*p as i32)))
}

/// Same license conservatism as scout auto-promotion: fail closed. A model is
/// auto-downloadable only when its license is present AND in the scout
/// allowlist — an absent/unknown license is treated exactly like a disallowed
/// one, so rows without license metadata require a manual `ff model download`.
pub fn license_allows_auto_download(license: &str) -> bool {
    if license.trim().is_empty() {
        return false;
    }
    let lower = license.trim().to_ascii_lowercase();
    crate::model_scout::ALLOWED_LICENSES
        .iter()
        .any(|l| *l == lower)
}

/// Catalog/library ids are interpolated into a shell command for the defer
/// queue, so only slug-safe ids may be auto-dispatched.
pub fn is_dispatchable_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Pick where to download: among nodes whose runtime has a matching variant
/// with a known size, require `free_disk >= size + BUILD_RESERVE_GB` (room to
/// store it without starving builds) and `free_ram >= size` (a node that
/// cannot load the model with its current occupancy is a dead-end copy —
/// total RAM is NOT the bar, what's left unused right now is), then take the
/// node with the most combined free disk + free RAM.
pub fn plan_download(variants: &JsonValue, nodes: &[NodeCapacity]) -> Option<PlannedDownload> {
    let variants = variants.as_array()?;
    let mut best: Option<(f64, PlannedDownload)> = None;
    for node in nodes {
        let Some(variant) = variants
            .iter()
            .find(|v| v.get("runtime").and_then(|x| x.as_str()) == Some(node.runtime.as_str()))
        else {
            continue;
        };
        let size_gb = variant
            .get("size_gb")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        if size_gb <= 0.0 {
            continue;
        }
        if node.free_disk_gb < size_gb + BUILD_RESERVE_GB || node.free_ram_gb < size_gb {
            continue;
        }
        let headroom = node.free_disk_gb + node.free_ram_gb;
        if best.as_ref().is_none_or(|(h, _)| headroom > *h) {
            best = Some((
                headroom,
                PlannedDownload {
                    node: node.name.clone(),
                    runtime: node.runtime.clone(),
                    size_gb,
                },
            ));
        }
    }
    best.map(|(_, plan)| plan)
}

/// One watchlist pass. The caller is expected to leader-gate (every node runs
/// the tick; only the live leader calls this), matching the auto-upgrade tick.
pub async fn reconcile_watchlist(pool: &PgPool) -> Result<WatchlistSummary, String> {
    let mut summary = enqueue_missing_downloads(pool).await?;
    let (registered, waiting) = register_challengers(pool).await?;
    summary.challengers_registered = registered;
    summary.challengers_waiting = waiting;
    Ok(summary)
}

/// Phase 1: fetch watchlisted models absent from the library fleet-wide.
async fn enqueue_missing_downloads(pool: &PgPool) -> Result<WatchlistSummary, String> {
    // Watchlisted, ungated, absent from the library fleet-wide, not already
    // in flight on the defer queue. `to_jsonb` keeps the license read working
    // whether or not the rich-metadata `license` column exists in this DB.
    let candidates = sqlx::query(
        r#"
        SELECT c.id, c.variants,
               COALESCE(to_jsonb(c) ->> 'license', '') AS license
          FROM fleet_model_catalog c
         WHERE c.watchlist
           AND NOT c.gated
           AND NOT EXISTS (
                 SELECT 1 FROM fleet_model_library l WHERE l.catalog_id = c.id
               )
           AND NOT EXISTS (
                 SELECT 1 FROM deferred_tasks d
                  WHERE d.status IN ('pending', 'running')
                    AND d.title LIKE 'watchlist-download: ' || c.id || ' on %'
               )
         ORDER BY c.tier DESC, c.id
         LIMIT $1
        "#,
    )
    .bind(MAX_DOWNLOADS_PER_PASS)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("watchlist candidate query: {e}"))?;

    let mut summary = WatchlistSummary {
        considered: candidates.len(),
        ..Default::default()
    };
    if candidates.is_empty() {
        return Ok(summary);
    }

    let nodes = fetch_node_capacity(pool).await?;

    for row in candidates {
        let id: String = row.get("id");
        let variants: JsonValue = row.get("variants");
        let license: String = row.get("license");

        if !license_allows_auto_download(&license) {
            info!(model = %id, license = %license, "watchlist skip: license missing or not in allowlist");
            summary.skipped += 1;
            continue;
        }
        if !is_dispatchable_id(&id) {
            warn!(model = %id, "watchlist skip: catalog id is not slug-safe");
            summary.skipped += 1;
            continue;
        }
        let Some(plan) = plan_download(&variants, &nodes) else {
            info!(model = %id, "watchlist skip: no online node fits est size + build reserve");
            summary.skipped += 1;
            continue;
        };

        // The download runs locally on the chosen node; its placement guard
        // re-checks disk against live `df` before streaming, and the
        // post-download library scan registers the row state='cold'.
        let command = format!("ff model download {} --runtime {}", id, plan.runtime);
        let title = format!("watchlist-download: {} on {}", id, plan.node);
        // Same 4h override as the cross-node download path — tens-of-GB pulls
        // outlive the task runner's 10-min default.
        let payload = json!({ "command": command, "max_duration_secs": 14400 });
        ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "now",
            &json!({}),
            Some(&plan.node),
            &json!([]),
            Some("autopilot-5-watchlist"),
            Some(3),
        )
        .await
        .map_err(|e| format!("enqueue watchlist download for {id}: {e}"))?;
        summary.enqueued += 1;
        info!(
            model = %id,
            node = %plan.node,
            runtime = %plan.runtime,
            size_gb = plan.size_gb,
            "watchlist download enqueued"
        );
    }
    Ok(summary)
}

/// Phase 2: register downloaded-but-idle watchlist models as bandit
/// challengers. Returns `(registered, waiting)`.
async fn register_challengers(pool: &PgPool) -> Result<(usize, usize), String> {
    // Watchlisted, ungated models that HAVE a library copy but no live
    // deployment and no in-flight challenger load. One library row per model
    // (the freshest download) — that node is where the load runs.
    let arrivals = sqlx::query(
        r#"
        SELECT c.id, c.tier, c.preferred_workloads,
               lib.id::text AS library_id, lib.worker_name
          FROM fleet_model_catalog c
          JOIN LATERAL (
                SELECT l.id, l.worker_name
                  FROM fleet_model_library l
                 WHERE l.catalog_id = c.id
                 ORDER BY l.downloaded_at DESC
                 LIMIT 1
               ) lib ON TRUE
         WHERE c.watchlist
           AND NOT c.gated
           AND NOT EXISTS (
                 SELECT 1 FROM fleet_model_deployments d
                  WHERE d.catalog_id = c.id AND d.health_status <> 'stopped'
               )
           AND NOT EXISTS (
                 SELECT 1 FROM deferred_tasks t
                  WHERE t.status IN ('pending', 'running')
                    AND t.title LIKE 'watchlist-challenger: ' || c.id || ' on %'
               )
         ORDER BY c.tier DESC, c.id
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("watchlist challenger arrivals query: {e}"))?;

    if arrivals.is_empty() {
        return Ok((0, 0));
    }

    // "Autopilot-4 landed" probe: its schema artifact is the
    // v_model_utilization reward view. Probing the object beats trusting
    // migration version numbers (V246 collided across branches).
    let autopilot4_landed: bool =
        sqlx::query_scalar("SELECT to_regclass('public.v_model_utilization') IS NOT NULL")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("autopilot-4 probe: {e}"))?;
    if !autopilot4_landed {
        info!(
            waiting = arrivals.len(),
            "watchlist: cold arrivals ready but Autopilot-4 bandit layer not present yet \
             (v_model_utilization missing); challenger registration resumes once it lands"
        );
        return Ok((0, arrivals.len()));
    }

    let mut registered = 0usize;
    let mut waiting = 0usize;
    for row in arrivals {
        let id: String = row.get("id");
        let tier: i32 = row.get("tier");
        let workloads_json: JsonValue = row.get("preferred_workloads");
        let library_id: String = row.get("library_id");
        let node: String = row.get("worker_name");

        let workloads: Vec<String> = workloads_json
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| w.as_str().map(str::to_string))
            .collect();
        if workloads.is_empty() {
            info!(model = %id, "watchlist: no preferred workloads; cannot pick an incumbent");
            waiting += 1;
            continue;
        }
        if !is_dispatchable_id(&library_id) {
            warn!(model = %id, library_id = %library_id, "watchlist: library id not slug-safe");
            waiting += 1;
            continue;
        }

        let candidates = fetch_incumbent_candidates(pool, &id, &workloads).await?;
        let Some(incumbent) = pick_incumbent(&candidates) else {
            info!(
                model = %id,
                "watchlist: no same-workload serving incumbent; challenger load deferred"
            );
            waiting += 1;
            continue;
        };
        let incumbent = incumbent.clone();

        // Tier + 0: enter the bandit group AT the incumbent's tier so
        // epsilon-greedy routing and the daily tier reconcile see the pair as
        // arms of the same (tier, workload) group.
        if let Some(new_tier) = align_challenger_tier(tier, incumbent.tier) {
            sqlx::query(
                "UPDATE fleet_model_catalog SET tier = $2, updated_at = NOW() WHERE id = $1",
            )
            .bind(&id)
            .bind(new_tier)
            .execute(pool)
            .await
            .map_err(|e| format!("align challenger tier for {id}: {e}"))?;
            info!(
                model = %id,
                incumbent = %incumbent.catalog_id,
                tier = new_tier,
                "watchlist: challenger tier aligned to incumbent (tier + 0)"
            );
        }

        let used_ports: Vec<i32> =
            sqlx::query_scalar("SELECT port FROM fleet_model_deployments WHERE worker_name = $1")
                .bind(&node)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("used ports on {node}: {e}"))?;
        let Some(port) = pick_challenger_port(&used_ports) else {
            warn!(model = %id, node = %node, "watchlist: no free challenger port on node");
            waiting += 1;
            continue;
        };

        let command = format!("ff model load {library_id} --port {port}");
        let title = format!("watchlist-challenger: {id} on {node}");
        // Big-model load + health wait can take many minutes; 1h headroom.
        let payload = json!({ "command": command, "max_duration_secs": 3600 });
        ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "now",
            &json!({}),
            Some(&node),
            &json!([]),
            Some("autopilot-5-watchlist"),
            Some(3),
        )
        .await
        .map_err(|e| format!("enqueue challenger load for {id}: {e}"))?;
        registered += 1;
        info!(
            model = %id,
            incumbent = %incumbent.catalog_id,
            node = %node,
            port,
            "watchlist: bandit challenger load enqueued at incumbent tier"
        );
    }
    Ok((registered, waiting))
}

/// Same-workload catalog models currently serving (healthy deployment), with
/// recent build volume from Autopilot-4's reward view. Only called after the
/// landed-probe confirms `v_model_utilization` exists.
async fn fetch_incumbent_candidates(
    pool: &PgPool,
    challenger_id: &str,
    workloads: &[String],
) -> Result<Vec<IncumbentCandidate>, String> {
    let rows = sqlx::query(
        r#"
        SELECT c.id, c.tier, COALESCE(u.builds, 0)::BIGINT AS builds
          FROM fleet_model_catalog c
          LEFT JOIN v_model_utilization u ON u.catalog_id = c.id
         WHERE c.id <> $1
           AND jsonb_exists_any(c.preferred_workloads, $2)
           AND EXISTS (
                 SELECT 1 FROM fleet_model_deployments d
                  WHERE d.catalog_id = c.id AND d.health_status = 'healthy'
               )
        "#,
    )
    .bind(challenger_id)
    .bind(workloads)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("incumbent candidates for {challenger_id}: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| IncumbentCandidate {
            catalog_id: r.get("id"),
            tier: r.get("tier"),
            builds: r.get("builds"),
        })
        .collect())
}

/// Online nodes with a known runtime, a fresh (≤1h) disk sample and a fresh
/// (≤1h) RAM heartbeat sample. A node that stopped sampling either signal is
/// treated as unavailable — those numbers are the only fit signal we have,
/// and stale ones over-promise.
///
/// Free RAM is `capacity - latest ram_used_gb` from `computer_metrics_history`
/// (the minutely heartbeat), joined via the canonical `computers.name =
/// fleet_workers.name` mapping; capacity prefers `computers.total_ram_gb` with
/// `fleet_workers.ram_gb` as fallback (same COALESCE as the leader election
/// eligibility query). Static total RAM alone is NOT a placement signal: a
/// 128GB box already serving 100GB of models has no room for a 48GB arrival.
const NODE_CAPACITY_SQL: &str = r#"
        SELECT w.name, w.runtime,
               GREATEST(
                   COALESCE(c.total_ram_gb, w.ram_gb, 0)::float8 - m.ram_used_gb,
                   0.0
               ) AS free_ram_gb,
               (d.free_bytes / 1073741824.0)::float8 AS free_disk_gb
          FROM fleet_workers w
          JOIN computers c ON c.name = w.name
          JOIN LATERAL (
                SELECT u.free_bytes
                  FROM fleet_disk_usage u
                 WHERE u.worker_name = w.name
                   AND u.sampled_at > NOW() - INTERVAL '1 hour'
                 ORDER BY u.sampled_at DESC
                 LIMIT 1
               ) d ON TRUE
          JOIN LATERAL (
                SELECT h.ram_used_gb
                  FROM computer_metrics_history h
                 WHERE h.computer_id = c.id
                   AND h.ram_used_gb IS NOT NULL
                   AND h.recorded_at > NOW() - INTERVAL '1 hour'
                 ORDER BY h.recorded_at DESC
                 LIMIT 1
               ) m ON TRUE
         WHERE w.status = 'online'
           AND w.runtime <> 'unknown'
"#;

async fn fetch_node_capacity(pool: &PgPool) -> Result<Vec<NodeCapacity>, String> {
    let rows = sqlx::query(NODE_CAPACITY_SQL)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("watchlist node capacity query: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| NodeCapacity {
            name: r.get("name"),
            runtime: r.get("runtime"),
            free_disk_gb: r.get("free_disk_gb"),
            free_ram_gb: r.get("free_ram_gb"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, runtime: &str, free_disk_gb: f64, free_ram_gb: f64) -> NodeCapacity {
        NodeCapacity {
            name: name.into(),
            runtime: runtime.into(),
            free_disk_gb,
            free_ram_gb,
        }
    }

    fn gguf_variants(size_gb: f64) -> JsonValue {
        json!([{ "runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "org/repo", "size_gb": size_gb }])
    }

    fn incumbent(id: &str, tier: i32, builds: i64) -> IncumbentCandidate {
        IncumbentCandidate {
            catalog_id: id.into(),
            tier,
            builds,
        }
    }

    #[test]
    fn plan_picks_most_combined_headroom() {
        let nodes = [
            node("small", "llama.cpp", 100.0, 32.0),
            node("big", "llama.cpp", 300.0, 128.0),
        ];
        let plan = plan_download(&gguf_variants(20.0), &nodes).unwrap();
        assert_eq!(plan.node, "big");
        assert_eq!(plan.runtime, "llama.cpp");
        assert_eq!(plan.size_gb, 20.0);
    }

    #[test]
    fn plan_requires_size_plus_build_reserve_on_disk() {
        // 20GB model + 30GB reserve = 50GB needed; 49GB free must not fit.
        let nodes = [node("tight", "llama.cpp", 49.9, 64.0)];
        assert!(plan_download(&gguf_variants(20.0), &nodes).is_none());
        let nodes = [node("fits", "llama.cpp", 50.1, 64.0)];
        assert!(plan_download(&gguf_variants(20.0), &nodes).is_some());
    }

    #[test]
    fn plan_requires_free_ram_to_load_the_model_now() {
        // 16GB currently free cannot host a 48GB load, no matter how big the
        // machine's total RAM is — occupancy, not capacity, is the bar.
        let nodes = [node("busy", "llama.cpp", 500.0, 16.0)];
        assert!(plan_download(&gguf_variants(48.0), &nodes).is_none());
        let nodes = [node("idle", "llama.cpp", 500.0, 48.0)];
        assert!(plan_download(&gguf_variants(48.0), &nodes).is_some());
    }

    #[test]
    fn plan_skips_unknown_size_and_runtime_mismatch() {
        // Unknown size → never blind-download.
        let nodes = [node("n1", "llama.cpp", 500.0, 128.0)];
        let no_size = json!([{ "runtime": "llama.cpp", "hf_repo": "org/repo" }]);
        assert!(plan_download(&no_size, &nodes).is_none());
        // No variant for the node's runtime → not placeable there.
        let mlx_only = [node("mac", "mlx", 500.0, 128.0)];
        assert!(plan_download(&gguf_variants(8.0), &mlx_only).is_none());
    }

    #[test]
    fn license_allowlist_matches_scout_conservatism() {
        // Fail closed: an absent/unknown license blocks auto-download, exactly
        // like the scout's missing-license filter.
        assert!(!license_allows_auto_download(""));
        assert!(!license_allows_auto_download("   "));
        assert!(license_allows_auto_download("mit"));
        assert!(license_allows_auto_download("Apache-2.0"));
        assert!(!license_allows_auto_download("proprietary"));
        assert!(!license_allows_auto_download("cc-by-nc-4.0"));
        assert!(!license_allows_auto_download("unknown"));
    }

    #[test]
    fn only_slug_safe_ids_are_dispatchable() {
        assert!(is_dispatchable_id("apriel-1.5-15b"));
        assert!(is_dispatchable_id("qwen3-coder-next-80b"));
        // Library UUIDs ride the same guard on the challenger-load path.
        assert!(is_dispatchable_id("2f4c0f3a-9f2f-4a68-93f7-4a1f8f3f2b10"));
        assert!(!is_dispatchable_id(""));
        assert!(!is_dispatchable_id("bad id; rm -rf /"));
        assert!(!is_dispatchable_id("a$(whoami)"));
    }

    #[test]
    fn incumbent_is_the_most_used_peer_with_deterministic_ties() {
        let peers = [
            incumbent("glm-4.5-air", 2, 40),
            incumbent("devstral-small", 2, 120),
            incumbent("qwen3-coder-30b", 3, 120),
        ];
        // Tie on builds breaks toward the lexicographically-lower id.
        assert_eq!(pick_incumbent(&peers).unwrap().catalog_id, "devstral-small");
        assert!(pick_incumbent(&[]).is_none());
    }

    #[test]
    fn challenger_enters_at_incumbent_tier_plus_zero() {
        // Different tier → align exactly to the incumbent's, never above.
        assert_eq!(align_challenger_tier(3, 2), Some(2));
        assert_eq!(align_challenger_tier(1, 2), Some(2));
        // Already aligned → no write.
        assert_eq!(align_challenger_tier(2, 2), None);
    }

    #[test]
    fn capacity_sql_derives_free_ram_from_live_occupancy() {
        // Regression guard: placement once read bare `fleet_workers.ram_gb`
        // (total RAM) and would download to nodes with no actual headroom.
        // The capacity query must subtract the live heartbeat occupancy.
        assert!(NODE_CAPACITY_SQL.contains("ram_used_gb"));
        assert!(NODE_CAPACITY_SQL.contains("computer_metrics_history"));
        assert!(NODE_CAPACITY_SQL.contains("free_ram_gb"));
        // And the sample must be freshness-gated, like the disk signal.
        assert_eq!(NODE_CAPACITY_SQL.matches("INTERVAL '1 hour'").count(), 2);
    }

    #[test]
    fn challenger_port_skips_taken_slots_and_exhausts() {
        assert_eq!(pick_challenger_port(&[]), Some(55000));
        assert_eq!(pick_challenger_port(&[55000, 55001]), Some(55002));
        let all: Vec<i32> = (55000..=55019).collect();
        assert_eq!(pick_challenger_port(&all), None);
    }
}
