//! Live interaction-log console API.
//!
//! Serves the `ff_interactions` table (V122) — the request/response corpus the
//! capture hooks fill (channels: `mcp`, `cli`, `gateway-jarvis`, `session`) — to
//! the web console at `/console`. Two endpoints, both always-200 with safe
//! defaults so the polling UI degrades gracefully when Postgres is unreachable:
//!
//!   - `GET /api/interactions?limit=&channel=` — recent rows, newest first.
//!   - `GET /api/interactions/summary`          — per-channel counts + total.
//!
//! Read-only; mirrors the pool-access + always-200 conventions in `jarvis_api`.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
    response::{Html, IntoResponse},
};
use serde::Deserialize;
use serde_json::json;

use crate::server::GatewayState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    /// Max rows (clamped 1..=500 in the DB layer). Default 100.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Filter to a single channel (mcp / cli / gateway-jarvis / session).
    #[serde(default)]
    pub channel: Option<String>,
}

/// GET /api/interactions — recent interaction-log rows, newest first.
pub async fn list_interactions(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<ListParams>,
) -> impl IntoResponse {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Json(json!({ "rows": [], "error": "database unavailable" }));
    };
    let limit = params.limit.unwrap_or(100);
    // Treat an empty/"all" channel as no filter.
    let channel = params
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("all"));

    match ff_db::pg_list_interactions(pool, limit, channel).await {
        Ok(rows) => Json(json!({ "rows": rows })),
        Err(e) => Json(json!({ "rows": [], "error": e.to_string() })),
    }
}

/// GET /api/interactions/summary — per-channel counts + grand total.
pub async fn interactions_summary(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Json(json!({ "channels": [], "total": 0, "error": "database unavailable" }));
    };
    match ff_db::pg_interaction_channel_counts(pool).await {
        Ok(channels) => {
            let total: i64 = channels
                .iter()
                .filter_map(|c| c.get("count").and_then(|v| v.as_i64()))
                .sum();
            Json(json!({ "channels": channels, "total": total }))
        }
        Err(e) => Json(json!({ "channels": [], "total": 0, "error": e.to_string() })),
    }
}

/// Serve the console page, compile-time-embedded from the git-tracked
/// `dashboard/public/console.html` (same `include_str!` approach as the JARVIS
/// HUD — `dist/` is gitignored and absent on remote hosts at build time, but
/// `public/` is always present).
pub async fn console_page() -> impl IntoResponse {
    Html(include_str!("../../../dashboard/public/console.html"))
}
