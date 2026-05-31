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
use std::time::Duration;

use axum::{Json, extract::State, response::IntoResponse};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::GatewayState;

/// JARVIS voice→fleet bridge timeout. Local models on memory-tight hosts can be
/// slow on first token; 60s is the generous ceiling before we fall back to a
/// canned reply. Mirrors the spirit of `fleet_offload`'s per-call timeout.
const JARVIS_ASK_TIMEOUT_SECS: u64 = 60;

/// JARVIS persona, prepended as the system message on every general-question
/// dispatch so the local model answers in-character. Concise, British,
/// anticipatory, addresses the user as "sir".
const JARVIS_SYSTEM_PROMPT: &str = "You are JARVIS, the voice assistant for \
the ForgeFleet distributed AI fleet. Reply in the persona of a concise, \
courteous, anticipatory British butler. Always address the user as \"sir\". \
Keep answers short enough to be spoken aloud — one or two sentences, no \
markdown, no lists, no preamble. Do not narrate your reasoning.";

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

/// Request body for `POST /api/jarvis/ask`.
#[derive(Debug, Deserialize)]
pub struct AskReq {
    /// The (already speech-to-text'd) user utterance.
    pub query: String,
}

/// POST /api/jarvis/ask — answer a JARVIS voice query with REAL data.
///
/// Response shape (always HTTP 200, JSON):
/// ```json
/// { "answer": "<concise spoken text>", "kind": "<intent>", "data": { … } | null }
/// ```
///
/// Intent is classified by lowercase substring match (same spirit as the HUD's
/// in-page `respond()`):
///   * status / systems / report / how are → live fleet summary from the same
///     DB queries `jarvis_state` uses (node counts, offload endpoint, schema
///     version, active tasks).
///   * fleet / computers / nodes → per-status node breakdown from `pg_list_nodes`.
///   * offload / credits / tokens → the warm endpoint from `pg_pick_offload_endpoint`.
///   * tasks / working on / building → active `fleet_tasks` count + top summary.
///   * else → routed to a warm local model over the OpenAI-compatible API with a
///     JARVIS system prompt, so the HUD gives genuine answers to free-form
///     questions. If nothing is warm, JARVIS says so in-character.
///
/// NULL-SAFE: like `jarvis_state`, never returns 5xx — a missing pool or a dead
/// endpoint degrades to an in-character spoken sentence.
pub async fn jarvis_ask(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<AskReq>,
) -> impl IntoResponse {
    let query = req.query.trim().to_string();
    let q = query.to_lowercase();

    // Pool access — exact pattern used by `jarvis_state` / pulse_api handlers.
    let pool = state.operational_store.as_ref().and_then(|os| os.pg_pool());

    // ── Intent: STATUS / SYSTEMS REPORT ───────────────────────────────────
    if contains_any(
        &q,
        &[
            "status",
            "systems",
            "report",
            "how are",
            "how's the",
            "sitrep",
        ],
    ) {
        if let Some(pool) = pool {
            let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();
            let total = nodes.len();
            let online = nodes
                .iter()
                .filter(|n| n.status.eq_ignore_ascii_case("online"))
                .count();
            let active_tasks = active_task_count(pool).await;
            let offload = ff_db::pg_pick_offload_endpoint(pool, 4096, None, &[])
                .await
                .ok()
                .flatten();
            let schema_version = schema_version(pool).await;

            let offload_phrase = match &offload {
                Some(c) => format!(
                    "a warm model on {} is standing by for offload",
                    c.worker_name
                ),
                None => "no local model is warm for offload at present".to_string(),
            };
            let answer = format!(
                "All systems nominal, sir. {online} of {total} computers online, \
                 {active_tasks} active task{}, and {offload_phrase}. Schema is at version {schema_version}.",
                if active_tasks == 1 { "" } else { "s" },
            );
            return jarvis_json(
                answer,
                "status",
                Some(json!({
                    "total_nodes": total,
                    "online_count": online,
                    "active_tasks": active_tasks,
                    "schema_version": schema_version,
                    "offload": offload.as_ref().map(|c| json!({
                        "endpoint": c.endpoint,
                        "worker_name": c.worker_name,
                        "model": c.catalog_name.clone().or_else(|| c.catalog_id.clone()),
                    })),
                })),
            );
        }
        return jarvis_json(
            "I'm afraid I can't reach the fleet database just now, sir.".to_string(),
            "status",
            None,
        );
    }

    // ── Intent: FLEET / COMPUTERS / NODES breakdown ───────────────────────
    if contains_any(&q, &["fleet", "computers", "nodes", "machines", "hosts"]) {
        if let Some(pool) = pool {
            let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();
            let total = nodes.len();
            let online = nodes
                .iter()
                .filter(|n| n.status.eq_ignore_ascii_case("online"))
                .count();
            let leader = leader_name(pool).await;
            let online_names: Vec<String> = nodes
                .iter()
                .filter(|n| n.status.eq_ignore_ascii_case("online"))
                .map(|n| n.name.clone())
                .collect();

            let leader_phrase = match &leader {
                Some(l) => format!(", led by {l}"),
                None => String::new(),
            };
            let answer =
                format!("The fleet has {total} computers, {online} online{leader_phrase}, sir.");
            return jarvis_json(
                answer,
                "fleet",
                Some(json!({
                    "total_nodes": total,
                    "online_count": online,
                    "leader": leader,
                    "online_nodes": online_names,
                    "nodes": nodes.iter().map(|n| json!({
                        "name": n.name,
                        "status": n.status,
                        "role": n.role,
                    })).collect::<Vec<_>>(),
                })),
            );
        }
        return jarvis_json(
            "I can't reach the fleet roster at the moment, sir.".to_string(),
            "fleet",
            None,
        );
    }

    // ── Intent: OFFLOAD / CREDITS / TOKENS ────────────────────────────────
    if contains_any(
        &q,
        &[
            "offload",
            "credits",
            "tokens",
            "warm model",
            "warm endpoint",
        ],
    ) {
        if let Some(pool) = pool {
            let offload = ff_db::pg_pick_offload_endpoint(pool, 16384, None, &[])
                .await
                .ok()
                .flatten();
            let (answer, data) = match offload {
                Some(c) => {
                    let model = c
                        .catalog_name
                        .clone()
                        .or_else(|| c.catalog_id.clone())
                        .unwrap_or_else(|| "a local model".to_string());
                    (
                        format!(
                            "Offload is warm on {}, sir — running {model}, and we're \
                             spending no cloud credits.",
                            c.worker_name
                        ),
                        json!({
                            "endpoint": c.endpoint,
                            "worker_name": c.worker_name,
                            "model": model,
                            "workload": c.family,
                            "status": "warm",
                        }),
                    )
                }
                None => (
                    "No local model is warm for offload right now, sir — load one \
                     with ff model load --agent."
                        .to_string(),
                    Value::Null,
                ),
            };
            return jarvis_json(answer, "offload", Some(data));
        }
        return jarvis_json(
            "I can't reach the offload router just now, sir.".to_string(),
            "offload",
            None,
        );
    }

    // ── Intent: TASKS / WORKING ON / BUILDING ─────────────────────────────
    if contains_any(&q, &["tasks", "working on", "building", "busy", "doing"]) {
        if let Some(pool) = pool {
            let active = active_task_count(pool).await;
            let top = top_task_name(pool).await;
            let answer = if active == 0 {
                "The fleet is idle, sir — no active tasks.".to_string()
            } else {
                match &top {
                    Some(name) => format!(
                        "{active} active task{}, sir. The top one is: {name}.",
                        if active == 1 { "" } else { "s" }
                    ),
                    None => format!(
                        "{active} active task{}, sir.",
                        if active == 1 { "" } else { "s" }
                    ),
                }
            };
            return jarvis_json(
                answer,
                "tasks",
                Some(json!({ "active_count": active, "top_name": top })),
            );
        }
        return jarvis_json(
            "I can't reach the task queue just now, sir.".to_string(),
            "tasks",
            None,
        );
    }

    // ── ELSE: route the question to a warm local model ────────────────────
    let answer = match pool {
        Some(pool) => dispatch_to_fleet(&state.http_client, pool, &query).await,
        None => "I have no warm local model to think with right now, sir — load \
                 one with ff model load --agent."
            .to_string(),
    };
    jarvis_json(answer, "ask", None)
}

/// Build the `POST /api/jarvis/ask` response envelope.
fn jarvis_json(answer: String, kind: &str, data: Option<Value>) -> Json<Value> {
    Json(json!({
        "answer": answer,
        "kind": kind,
        "data": data.unwrap_or(Value::Null),
    }))
}

/// `true` if `haystack` contains any of `needles` (all assumed already lowercase).
fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Active (in-flight) `fleet_tasks` count — same predicate as `jarvis_state`.
async fn active_task_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM fleet_tasks WHERE status IN ('pending', 'claimed', 'running')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0)
}

/// Summary of the highest-priority active task, if any — same query as `jarvis_state`.
async fn top_task_name(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT summary FROM fleet_tasks \
         WHERE status IN ('pending', 'claimed', 'running') \
         ORDER BY priority DESC, created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Current HA leader member name, if recorded.
async fn leader_name(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT member_name FROM fleet_leader_state LIMIT 1")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Highest applied schema migration version, as a spoken-friendly string.
/// Mirrors `status_cmd`'s query against the `_migrations` table (the Postgres
/// schema-version source of truth; the version column is `i32`, cast to bigint).
async fn schema_version(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(version), 0)::bigint FROM _migrations")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Route a free-form question to a warm, tool-capable local model over the
/// OpenAI-compatible API and return the (think-stripped) assistant text.
///
/// This is the SAME dispatch primitive `fleet_offload` uses:
///   1. `pg_pick_offload_endpoint(pool, 16384, kind, &[])` picks the best warm
///      endpoint (no parallel router).
///   2. POST `/v1/chat/completions` with a JARVIS system prompt + the user
///      query, `chat_template_kwargs.enable_thinking=false`, a 60s timeout.
///   3. Strip any `<think>…</think>` and return the assistant content.
/// No warm endpoint → an in-character "no model to think with" line.
async fn dispatch_to_fleet(client: &reqwest::Client, pool: &sqlx::PgPool, query: &str) -> String {
    // `kind = None` → any warm tool-capable model (this is a chat question, not
    // a coding dispatch). 16384 matches fleet_offload's OFFLOAD_DEFAULT_MIN_CTX.
    let candidate = match ff_db::pg_pick_offload_endpoint(pool, 16384, None, &[]).await {
        Ok(Some(c)) => c,
        _ => {
            return "I have no warm local model to think with right now, sir — \
                    load one with ff model load --agent."
                .to_string();
        }
    };

    let model = candidate
        .catalog_id
        .clone()
        .or_else(|| candidate.catalog_name.clone())
        .unwrap_or_default();
    let url = format!(
        "{}/v1/chat/completions",
        candidate.endpoint.trim_end_matches('/')
    );

    // Same request shape as fleet_offload: enable_thinking=false so the model
    // returns the answer (not chain-of-thought), bounded output, no streaming.
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": JARVIS_SYSTEM_PROMPT },
            { "role": "user", "content": query },
        ],
        "stream": false,
        "max_tokens": 512,
        "temperature": 0.4,
        "chat_template_kwargs": { "enable_thinking": false },
    });

    let resp = client
        .post(&url)
        .timeout(Duration::from_secs(JARVIS_ASK_TIMEOUT_SECS))
        .json(&body)
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, endpoint = %candidate.endpoint, "jarvis_ask dispatch failed");
            return "I couldn't reach the local model just now, sir — do try again.".to_string();
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        tracing::warn!(%status, endpoint = %candidate.endpoint, "jarvis_ask endpoint returned non-2xx");
        return format!(
            "The local model on {} returned an error, sir.",
            candidate.worker_name
        );
    }

    let payload: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "jarvis_ask failed parsing endpoint JSON");
            return "I received an unreadable reply from the model, sir.".to_string();
        }
    };

    let raw = extract_completion_text(&payload).unwrap_or_default();
    let text = strip_think_block(&raw);
    if text.is_empty() {
        "I'm not certain how to answer that, sir.".to_string()
    } else {
        text
    }
}

/// Pull the assistant text out of an OpenAI-compatible chat-completion payload.
/// Self-contained mirror of `ff-mcp`'s `extract_completion_text` so this module
/// has no cross-crate dependency on a private helper.
fn extract_completion_text(payload: &Value) -> Option<String> {
    payload
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| {
            choice
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    choice
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
        })
}

/// Strip any `<think>…</think>` reasoning, returning the trimmed remainder.
/// Belt-and-suspenders with `chat_template_kwargs.enable_thinking=false`.
/// Kept in sync with `strip_think_block` in `ff-mcp/src/handlers.rs`.
fn strip_think_block(s: &str) -> String {
    let mut out = s.to_string();
    // 1) Remove well-formed <think>…</think> pairs, left-to-right.
    loop {
        let Some(open) = out.find("<think>") else {
            break;
        };
        match out[open..].find("</think>") {
            Some(rel) => {
                let close = open + rel + "</think>".len();
                out.replace_range(open..close, "");
            }
            // 2) Unclosed opener — reasoning cut off under a token cap; drop it.
            None => {
                out.truncate(open);
                break;
            }
        }
    }
    // 3) A lone trailing </think> with no opener: the answer follows it.
    if let Some(i) = out.rfind("</think>") {
        out = out[i + "</think>".len()..].to_string();
    }
    out.trim().to_string()
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
