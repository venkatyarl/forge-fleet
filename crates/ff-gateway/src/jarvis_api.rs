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
use sqlx::Row;

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
            "missions": [],
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

    // ─── missions ─────────────────────────────────────────────────────
    // The most-recent active agent sessions JARVIS is driving, so the HUD's
    // ACTIVE MISSIONS card can show live progress. Best-effort: any failure
    // leaves an empty array (the HUD guards on `state.missions || []`).
    let missions = recent_missions(pool).await;

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
        "missions": missions,
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

    // ── Intent: ACTION REQUEST — start a fleet mission ────────────────────
    // An imperative/task-like utterance ("build X", "fix Y", "please add Z")
    // becomes a real agent session that the production daemon's
    // session_runner executes across the fleet. Questions stay on the answer
    // path below; when ambiguous, is_action_request() prefers ANSWER, since
    // acting is the higher-consequence choice.
    if is_action_request(&q) {
        if let Some(pool) = pool {
            match start_mission(pool, &query).await {
                Ok(mission_id) => {
                    return jarvis_action_json(
                        "On it. I've started a mission across the fleet — I'll work \
                         on it and report progress on your display."
                            .to_string(),
                        mission_id.to_string(),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "jarvis_ask failed to start mission");
                    return jarvis_json(
                        "I'm afraid I couldn't start that mission just now, sir — \
                         the fleet database refused the request."
                            .to_string(),
                        "action",
                        None,
                    );
                }
            }
        }
        return jarvis_json(
            "I can't reach the fleet to start that just now, sir.".to_string(),
            "action",
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

/// Classify a lowercased utterance as an ACTION request (imperative/task-like)
/// vs a question. Conservative by design — when the signal is ambiguous we
/// return `false` so the caller takes the lower-consequence ANSWER path.
///
/// Rules (checked in order):
///   1. Clear interrogatives at the start (`what`, `how`, `is`, `show`, …) or a
///      trailing `?` → NOT an action. Question wins even if a verb appears
///      ("what should I build?" is a question).
///   2. Polite/explicit action lead-ins ("please …", "can you build …",
///      "go ahead and …") → action.
///   3. A build/do verb appearing as a word anywhere → action.
///   4. Otherwise → not an action.
fn is_action_request(q: &str) -> bool {
    let q = q.trim();
    if q.is_empty() {
        return false;
    }

    // (1) Questions win. A trailing '?' or a leading interrogative keeps us on
    // the ANSWER path regardless of any verb in the sentence.
    if q.ends_with('?') {
        return false;
    }
    let first_word = q.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
    const QUESTION_LEADS: &[&str] = &[
        "what", "why", "how", "who", "when", "where", "which", "whose", "is", "are", "am", "was",
        "were", "do", "does", "did", "can", "could", "would", "should", "will", "shall", "show",
        "list", "status", "tell", "explain", "describe", "report",
    ];
    // "can/could/would/should/will" lead a question UNLESS followed by "you" +
    // an action verb ("can you build …"), which (2) below treats as action.
    let polite_action = matches!(first_word, "can" | "could" | "would" | "will")
        && q.split_whitespace()
            .nth(1)
            .map(str::to_lowercase)
            .as_deref()
            == Some("you")
        && q.split_whitespace()
            .nth(2)
            .map(|w| ACTION_VERBS.iter().any(|v| w.to_lowercase().starts_with(v)))
            .unwrap_or(false);
    if QUESTION_LEADS.contains(&first_word) && !polite_action {
        return false;
    }

    // (2) Explicit action lead-ins (and the two-word "set up …" phrase that
    // can't be a single verb stem).
    let ql = q.to_lowercase();
    if ql.starts_with("please ")
        || ql.starts_with("go ahead")
        || ql.contains("set up ")
        || polite_action
    {
        return true;
    }

    // (3) A build/do verb as a standalone word anywhere in the utterance.
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| {
            let w = w.to_lowercase();
            ACTION_VERBS.contains(&w.as_str())
        })
}

/// Imperative build/do verbs that mark an utterance as an ACTION request.
/// Kept as bare stems; the classifier matches whole words (and `starts_with`
/// for the "can you <verb>" lead-in so "building"/"creates" also catch).
const ACTION_VERBS: &[&str] = &[
    "build",
    "fix",
    "add",
    "implement",
    "refactor",
    "create",
    "write",
    "test",
    "deploy",
    "migrate",
    "optimize",
    "optimise",
    "run",
    "generate",
    "setup", // bare "set" is too broad; we accept "setup" and the "set up" phrase below
    "make",
    "update",
    "upgrade",
    "install",
];

/// Number of whitespace-delimited words at/above which a JARVIS mission is
/// treated as non-trivial enough to warrant LLM decomposition (Orchestrator
/// P4) rather than a single primary step. Deliberately conservative: a short
/// spoken command ("restart marcus", "scan the disk") stays single-step;
/// a real build request ("build a CLI that imports CSVs, validates them, and
/// writes a Postgres table with a migration") crosses the threshold and
/// fans out into a parallel team.
const DECOMPOSE_WORD_THRESHOLD: usize = 8;

/// Create a session for `goal`. For a non-trivial goal (>= the word
/// threshold) this takes the **plan-first** path: a `planner` step is
/// attached and `session_runner::tick()` auto-fans it out into a parallel
/// child DAG (Orchestrator P4) with no manual apply-plan step. Short/trivial
/// goals keep the original single primary-step behaviour.
///
/// Mirrors the `ff session spawn` / `ff session plan` call shapes. Returns the
/// new session id (the HUD's `mission_id`).
async fn start_mission(pool: &sqlx::PgPool, goal: &str) -> anyhow::Result<uuid::Uuid> {
    let word_count = goal.split_whitespace().count();
    if word_count >= DECOMPOSE_WORD_THRESHOLD {
        // Plan-first: create_decomposed_session does create_session +
        // add_planner_step; tick() folds the planner's completion into
        // child steps automatically and dispatches them in parallel.
        return ff_agent::session_runner::create_decomposed_session(pool, goal, Some("jarvis"))
            .await;
    }

    // Trivial goal — single primary step, unchanged.
    // create_session(pool, goal, team=None, budget_usd_cap=None, created_by).
    let session_id =
        ff_agent::session_runner::create_session(pool, goal, None, None, Some("jarvis")).await?;
    // add_step(pool, session_id, name, role=None, prompt, depends_on=&[]).
    // role=None lets the runner fall back to its default coder model; the
    // single primary step's prompt is the user's request verbatim.
    ff_agent::session_runner::add_step(pool, session_id, "primary", None, goal, &[]).await?;
    Ok(session_id)
}

/// Build the `POST /api/jarvis/ask` response envelope for an ACTION request.
/// Same base shape as `jarvis_json` (so the HUD's `respond()` still finds
/// `answer`/`kind`), with an added top-level `mission_id` and `intent`.
fn jarvis_action_json(answer: String, mission_id: String) -> Json<Value> {
    Json(json!({
        "answer": answer,
        "kind": "action",
        "intent": "action",
        "mission_id": mission_id,
        "data": Value::Null,
    }))
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

/// The most-recent active agent sessions (the "missions" JARVIS is driving),
/// for the HUD's ACTIVE MISSIONS card. We use a direct `sqlx::query` against
/// `agent_sessions` + `agent_steps` rather than
/// `session_runner::list_sessions` because the HUD needs status/time
/// filtering plus a `steps_running` counter and an `updated_at` that
/// `list_sessions` doesn't expose — and a tailored query is simpler than
/// post-filtering its richer-but-wrong shape.
///
/// Selection: non-terminal status (`pending`/`running`) created in the last
/// 6 hours, newest first, capped at 6. Best-effort — any query error yields an
/// empty array so the state response stays null-safe.
async fn recent_missions(pool: &sqlx::PgPool) -> Vec<Value> {
    let rows = sqlx::query(
        "SELECT s.id,
                s.goal,
                s.status,
                COALESCE(s.completed_at, s.started_at, s.created_at) AS updated_at,
                COUNT(st.id)                                          AS total,
                COUNT(st.id) FILTER (WHERE st.status = 'completed')   AS done,
                COUNT(st.id) FILTER (WHERE st.status = 'running')     AS running
           FROM agent_sessions s
           LEFT JOIN agent_steps st ON st.session_id = s.id
          WHERE s.status IN ('pending', 'running')
            AND s.created_at > NOW() - INTERVAL '6 hours'
          GROUP BY s.id
          ORDER BY s.created_at DESC
          LIMIT 6",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|r| {
            json!({
                "id":            r.get::<uuid::Uuid, _>("id").to_string(),
                "goal":          r.get::<String, _>("goal"),
                "status":        r.get::<String, _>("status"),
                "steps_total":   r.get::<i64, _>("total"),
                "steps_done":    r.get::<i64, _>("done"),
                "steps_running": r.get::<i64, _>("running"),
                "updated_at":    r
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
                    .ok()
                    .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            })
        })
        .collect()
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
    let started = std::time::Instant::now();
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
    let answer = if text.is_empty() {
        "I'm not certain how to answer that, sir.".to_string()
    } else {
        text
    };

    // Interaction-log capture: channel="gateway-jarvis". Spawned so the DB
    // write never delays the HTTP response. Mirrors the fleet_run hook.
    {
        let pool = pool.clone();
        let query_owned = query.to_string();
        let answer_owned = answer.clone();
        let engine_owned = model.clone();
        let latency_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
        let tokens_in = payload
            .get("usage")
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        let tokens_out = payload
            .get("usage")
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        tokio::spawn(async move {
            let rec = ff_db::InteractionRecord {
                channel: "gateway-jarvis".to_string(),
                request_text: query_owned,
                engine: Some(engine_owned),
                response_text: answer_owned,
                tokens_in,
                tokens_out,
                latency_ms: Some(latency_ms),
                outcome: "ok".to_string(),
                ..Default::default()
            };
            if let Err(e) = ff_db::pg_record_interaction(&pool, &rec).await {
                tracing::debug!("jarvis dispatch interaction capture failed: {e}");
            }
        });
    }

    answer
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Imperative/task-like utterances must classify as ACTIONS (`true`).
    #[test]
    fn classifies_actions_as_action() {
        for q in [
            "build a readme",
            "fix the bug in main.rs",
            "implement the cache",
            "refactor this",
            "deploy to taylor",
            "run the tests",
            "please add a test",               // "please " lead-in
            "go ahead and migrate the schema", // "go ahead" lead-in
            "set up CI",                       // "set up " two-word phrase
            "can you build a hello world",     // polite_action: can/you/<verb>
        ] {
            assert!(
                is_action_request(q),
                "expected ACTION (true) for {q:?}, got false"
            );
        }
    }

    /// Questions (and ambiguous utterances) must classify as NOT-action
    /// (`false`) so the caller takes the lower-consequence ANSWER path.
    #[test]
    fn classifies_questions_as_not_action() {
        for q in [
            "what is the fleet status?",
            "how many nodes are online?",
            "why did the build fail?",
            "is marcus online?",
            "show me the deployments", // "show" is a QUESTION_LEAD
            "list the tasks",          // "list" is a QUESTION_LEAD
            "who is the leader?",
            "can you tell me the status?", // trailing '?' wins; also "tell" is not an ACTION_VERB
            "could you explain the orchestrator?",
        ] {
            assert!(
                !is_action_request(q),
                "expected QUESTION (false) for {q:?}, got true"
            );
        }
    }

    /// Edge cases pinned to the IMPLEMENTATION's actual behavior (regression
    /// fence, not a re-design).
    #[test]
    fn edge_cases_match_impl() {
        // Bare "status" — first word IS a QUESTION_LEAD, so it short-circuits
        // to a question even though there's no '?'. => false.
        assert!(!is_action_request("status"));

        // "build status" — first word "build" is NOT a QUESTION_LEAD, so the
        // question short-circuit (rule 1) does not fire; rule 3 then matches
        // "build" as an ACTION_VERB anywhere in the utterance. => true.
        // (Implementation prioritizes verb-presence over the trailing "status"
        // noun once the leading word isn't interrogative.)
        assert!(is_action_request("build status"));

        // Empty / whitespace-only input is explicitly handled as not-action.
        assert!(!is_action_request(""));
        assert!(!is_action_request("   "));
    }
}
