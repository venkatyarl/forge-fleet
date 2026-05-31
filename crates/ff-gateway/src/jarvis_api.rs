//! JARVIS HUD live-data endpoint.
//!
//! Mounted in server.rs as `GET /api/jarvis/state`. Returns a single,
//! version-tagged JSON snapshot that the standalone HUD
//! (`dashboard/public/jarvis.html`) polls every few seconds to drive its
//! gauges, node dots, subsystem rows, and section cards.
//!
//! Design contract (`schema_version: "1"`):
//! ```json
//! {
//!   "schema_version": "1",
//!   "timestamp": "2026-05-31T...Z",
//!   "fleet": { "total_nodes", "online_count", "leader", "gateway_version" },
//!   "nodes": [ { "name", "status", "role", "total_ram_gb",
//!                "models_loaded", "heartbeat_age_sec" } ],
//!   "deployments": [ { "worker_name", "catalog_id", "port",
//!                      "workload", "health_status" } ],
//!   "offload": { "endpoint", "worker_name", "model", "workload", "status" }
//!               | null,
//!   "tasks": { "active_count", "top_name" },
//!   "brain": { "linked": bool },
//!   "subsystems": { "orchestrator", "offload", "reconciler", "agents",
//!                   "brain" }
//! }
//! ```
//!
//! NULL-SAFE: if Postgres is unavailable the handler returns HTTP 200 with
//! sensible defaults (`offload: null`, every subsystem `"unknown"`, empty
//! arrays) — it NEVER returns 5xx, so the HUD never black-screens. It also
//! mirrors the `iso()` timestamp + `json!` shapes used in `pulse_api.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{Json, extract::State, response::IntoResponse};
use serde_json::{Value, json};

use crate::server::GatewayState;

/// RFC3339 UTC timestamp with second precision — same helper shape as
/// `pulse_api::iso`, kept local so this module is self-contained.
fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// GET /api/jarvis/state — aggregate live fleet state for the JARVIS HUD.
///
/// Always 200. When Postgres is reachable it composes `pg_list_nodes`,
/// `pg_list_deployments`, `pg_pick_offload_endpoint`,
/// `pg_active_deployment_counts`, the leader row and a brain-vault probe
/// into the contract above; when it isn't, it returns the same shape with
/// defaults so the front-end degrades gracefully.
pub async fn jarvis_state(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    let gateway_version = ff_core::VERSION.to_string();

    // Pool access — exact pattern used by pulse_api / server.rs handlers.
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        // Postgres unavailable: 200 with safe defaults, never 503.
        return Json(json!({
            "schema_version": "1",
            "timestamp": iso_now(),
            "fleet": {
                "total_nodes": 0,
                "online_count": 0,
                "leader": Value::Null,
                "gateway_version": gateway_version,
            },
            "nodes": [],
            "deployments": [],
            "offload": Value::Null,
            "tasks": { "active_count": 0, "top_name": Value::Null },
            "brain": { "linked": false },
            "subsystems": {
                "orchestrator": "unknown",
                "offload": "unknown",
                "reconciler": "unknown",
                "agents": "unknown",
                "brain": "unknown",
            },
        }));
    };

    // ─── nodes ────────────────────────────────────────────────────────
    let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();

    // ─── deployments ──────────────────────────────────────────────────
    let deployments = ff_db::pg_list_deployments(pool, None)
        .await
        .unwrap_or_default();

    // models_loaded per worker — count of active deployments keyed by name.
    let mut models_by_worker: HashMap<String, i64> = HashMap::new();
    for d in &deployments {
        *models_by_worker.entry(d.worker_name.clone()).or_insert(0) += 1;
    }

    // ─── heartbeat ages ───────────────────────────────────────────────
    // `FleetNodeRow` carries no last-seen timestamp; the freshness lives on
    // the `computers` table. One cheap lookup keyed by name keeps the node
    // shape's `heartbeat_age_sec` honest without an N+1 per node.
    let heartbeat_ages: HashMap<String, i64> = sqlx::query_as::<
        _,
        (String, Option<chrono::DateTime<chrono::Utc>>),
    >("SELECT name, last_seen_at FROM computers")
    .fetch_all(pool)
    .await
    .map(|rows| {
        let now = chrono::Utc::now();
        rows.into_iter()
            .filter_map(|(name, seen)| seen.map(|t| (name, (now - t).num_seconds())))
            .collect()
    })
    .unwrap_or_default();

    let online_count = nodes
        .iter()
        .filter(|n| n.status.eq_ignore_ascii_case("online"))
        .count();

    let nodes_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            // Prefer the true hardware RAM from `computers` when present.
            let total_ram_gb = n.computer_ram_gb.unwrap_or(n.ram_gb);
            json!({
                "name": n.name,
                "status": n.status,
                "role": n.role,
                "total_ram_gb": total_ram_gb,
                "models_loaded": models_by_worker.get(&n.name).copied().unwrap_or(0),
                "heartbeat_age_sec": heartbeat_ages.get(&n.name).copied(),
            })
        })
        .collect();

    let deployments_json: Vec<Value> = deployments
        .iter()
        .map(|d| {
            json!({
                "worker_name": d.worker_name,
                "catalog_id": d.catalog_id,
                "port": d.port,
                // No per-deployment workload column on `fleet_model_deployments`;
                // surface the runtime so the HUD has something to tag the row with.
                "workload": d.runtime,
                "health_status": d.health_status,
            })
        })
        .collect();

    // ─── leader ───────────────────────────────────────────────────────
    let leader: Option<String> =
        sqlx::query_scalar::<_, String>("SELECT member_name FROM fleet_leader_state LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    // ─── offload endpoint (null-safe) ─────────────────────────────────
    // 4096 is the low chat-tier ctx floor (code dispatch asks for more); kind
    // None = "any warm tool-capable model"; no excludes. Mirrors the offload
    // selector both `ff offload` and `fleet_offload` use.
    let offload_pick = ff_db::pg_pick_offload_endpoint(pool, 4096, None, &[])
        .await
        .ok()
        .flatten();
    let offload_json = match &offload_pick {
        Some(c) => json!({
            "endpoint": c.endpoint,
            "worker_name": c.worker_name,
            "model": c.catalog_name.clone().or_else(|| c.catalog_id.clone()),
            "workload": c.family,
            "status": "warm",
        }),
        None => Value::Null,
    };

    // ─── tasks ────────────────────────────────────────────────────────
    // Active = in-flight fleet_tasks (pending/claimed/running per the schema's
    // status enum); top_name surfaces the `summary` of the most-recent active
    // one. Best-effort: any query failure leaves the defaults.
    let active_count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM fleet_tasks WHERE status IN ('pending', 'claimed', 'running')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let top_name: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT summary FROM fleet_tasks \
         WHERE status IN ('pending', 'claimed', 'running') \
         ORDER BY priority DESC, created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    // ─── brain ────────────────────────────────────────────────────────
    // "Linked" = the brain vault has any indexed node. A failed/empty probe
    // simply reports not-linked rather than erroring the whole response.
    let brain_node_count: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM brain_vault_nodes")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    let brain_linked = brain_node_count > 0;

    // ─── subsystems (derived from live state) ─────────────────────────
    // Since Postgres is reachable here, the orchestrator/reconciler are
    // assumed nominal; offload reflects whether a warm endpoint exists;
    // agents reflects whether any tool-capable deployment is healthy; brain
    // reflects the vault link. Each is an ok|warn|unknown enum string.
    let agents_state = if deployments
        .iter()
        .any(|d| d.health_status.eq_ignore_ascii_case("healthy"))
    {
        "ok"
    } else {
        "warn"
    };
    let subsystems = json!({
        "orchestrator": "ok",
        "offload": if offload_pick.is_some() { "ok" } else { "warn" },
        "reconciler": "ok",
        "agents": agents_state,
        "brain": if brain_linked { "ok" } else { "warn" },
    });

    Json(json!({
        "schema_version": "1",
        "timestamp": iso_now(),
        "fleet": {
            "total_nodes": nodes.len(),
            "online_count": online_count,
            "leader": leader,
            "gateway_version": gateway_version,
        },
        "nodes": nodes_json,
        "deployments": deployments_json,
        "offload": offload_json,
        "tasks": {
            "active_count": active_count,
            "top_name": top_name,
        },
        "brain": { "linked": brain_linked },
        "subsystems": subsystems,
    }))
}

/// Serve the JARVIS HUD itself, compile-time-embedded from the git-tracked
/// `dashboard/public/jarvis.html`. Served same-origin as `/api/jarvis/state`
/// so the live-data poller needs no CORS. We embed via `include_str!` (not the
/// rust-embed dashboard/dist path) because `dist/` is gitignored and the deploy
/// runs `cargo build` only — so dist/ is never present on remote hosts at
/// compile time, but `public/` always is.
pub async fn jarvis_hud() -> impl IntoResponse {
    axum::response::Html(include_str!("../../../dashboard/public/jarvis.html"))
}
