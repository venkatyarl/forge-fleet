use axum::{body::Body, extract::State, http::StatusCode, response::Response, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::server::GatewayState;

// ═══════════════════════════════════════════════════════════════════════════════
//  Request types
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct TaskRequest {
    #[serde(default)]
    pub task: String,
    pub input: Value,
    #[serde(default)]
    pub output_format: Option<String>, // "json" or "text"
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub context: Option<String>, // extra context for the task
}

#[derive(Debug, Deserialize)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AudioTranscriptionRequest {
    pub audio_base64: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Task definitions
// ═══════════════════════════════════════════════════════════════════════════════

struct TaskDef {
    task: &'static str,
    capabilities: &'static [&'static str],
    system_prompt: Option<&'static str>,
}

const TASK_DEFINITIONS: &[TaskDef] = &[
    TaskDef {
        task: "chat",
        capabilities: &["chat"],
        system_prompt: None,
    },
    TaskDef {
        task: "summarize",
        capabilities: &["chat", "long_context"],
        system_prompt: Some(
            "You are a concise summarization assistant. Summarize the provided content clearly and accurately. Output only the summary, no preamble.",
        ),
    },
    TaskDef {
        task: "extract",
        capabilities: &["chat", "tool_calling"],
        system_prompt: Some(
            "You are a structured data extraction assistant. Extract the requested information and return it as valid JSON. Do not include markdown code blocks or explanations outside the JSON.",
        ),
    },
    TaskDef {
        task: "generate",
        capabilities: &["chat", "reasoning"],
        system_prompt: Some(
            "You are a creative assistant. Generate high-quality content based on the user's request.",
        ),
    },
    TaskDef {
        task: "code",
        capabilities: &["code", "tool_calling"],
        system_prompt: Some(
            "You are an expert programming assistant. Write clean, well-documented, production-ready code. Include comments for complex logic.",
        ),
    },
    TaskDef {
        task: "vision",
        capabilities: &["vision", "chat"],
        system_prompt: Some(
            "You are a computer vision assistant. Describe and analyze images in detail.",
        ),
    },
    TaskDef {
        task: "classify",
        capabilities: &["chat", "reasoning"],
        system_prompt: Some(
            "You are a classification assistant. Classify the input and return only the label.",
        ),
    },
    TaskDef {
        task: "translate",
        capabilities: &["chat"],
        system_prompt: Some(
            "You are a translation assistant. Translate accurately while preserving meaning and tone. Output only the translation.",
        ),
    },
];

fn get_task_def(task: &str) -> Option<&'static TaskDef> {
    TASK_DEFINITIONS
        .iter()
        .find(|t| t.task.eq_ignore_ascii_case(task))
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Payload builder
// ═══════════════════════════════════════════════════════════════════════════════

fn build_chat_payload(task_def: &TaskDef, req: &TaskRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(prompt) = task_def.system_prompt {
        messages.push(json!({ "role": "system", "content": prompt }));
    }

    let mut user_content = String::new();
    if let Some(ref ctx) = req.context {
        user_content.push_str(ctx);
        user_content.push('\n');
        user_content.push('\n');
    }
    let input_text = match &req.input {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    user_content.push_str(&input_text);

    messages.push(json!({ "role": "user", "content": user_content }));

    let mut payload = json!({
        "messages": messages,
        "stream": req.stream.unwrap_or(false),
    });

    if let Some(ref model) = req.model
        && !model.eq_ignore_ascii_case("auto") {
            payload["model"] = json!(model);
        }

    if req.output_format.as_deref() == Some("json") {
        payload["response_format"] = json!({ "type": "json_object" });
    }

    payload
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════════

fn rewrite_endpoint(endpoint: &str, primary_ip: &str) -> String {
    if primary_ip.is_empty() {
        return endpoint.to_string();
    }
    endpoint
        .replace("127.0.0.1", primary_ip)
        .replace("localhost", primary_ip)
}

fn collect_available_capabilities(
    catalog_entries: &HashMap<String, (String, i32, Value)>,
) -> Vec<String> {
    let mut caps = std::collections::HashSet::new();
    for (_, _, pw) in catalog_entries.values() {
        if let Some(arr) = pw.as_array() {
            for v in arr {
                if let Some(s) = v.as_str() {
                    caps.insert(s.to_string());
                }
            }
        }
    }
    let mut result: Vec<String> = caps.into_iter().collect();
    result.sort();
    result
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Main task handler
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn handle_task(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<TaskRequest>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    handle_task_inner(state, req.task.clone(), req).await
}

pub async fn handle_task_from_path(
    State(state): State<Arc<GatewayState>>,
    axum::extract::Path(task_type): axum::extract::Path<String>,
    Json(req): Json<TaskRequest>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    handle_task_inner(state, task_type, req).await
}

async fn handle_task_inner(
    state: Arc<GatewayState>,
    task_type: String,
    req: TaskRequest,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    let task_def = match get_task_def(&task_type) {
        Some(td) => td,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!("unknown task type '{}'", task_type),
                        "type": "invalid_request_error",
                        "available_tasks": TASK_DEFINITIONS.iter().map(|t| t.task).collect::<Vec<_>>(),
                    }
                })),
            ));
        }
    };

    let mut body = build_chat_payload(task_def, &req);

    // ── 3a. Direct model routing if a specific model was requested ───────────
    if let Some(ref model) = req.model
        && !model.eq_ignore_ascii_case("auto")
            && let Some(ref router) = state.pulse_router {
                let cache = state.pulse_cache.as_deref();
                let pg = state.operational_store.as_ref().and_then(|s| s.pg_pool());
                match router.route_completion_cached(body.clone(), cache, pg).await {
                    Ok(result) => {
                        info!(task = %task_type, model = %model, "task routed directly via pulse router");
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "application/json")
                            .body(Body::from(result.to_string()))
                            .map_err(|e| {
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(json!({
                                        "error": {
                                            "message": e.to_string(),
                                            "type": "internal_error",
                                        }
                                    })),
                                )
                            });
                    }
                    Err(crate::llm_routing::LlmRoutingError::NoMatch { .. })
                    | Err(crate::llm_routing::LlmRoutingError::MissingModel) => {
                        debug!(
                            task = %task_type,
                            model = %model,
                            "direct pulse routing missed, falling back to capability routing"
                        );
                    }
                    Err(e) => {
                        let (code, err_body) = crate::llm_routing::error_to_response(e);
                        return Err((
                            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                            Json(err_body),
                        ));
                    }
                }
            }

    // ── 3b. Capability-based fleet routing ───────────────────────────────────
    let pool = match state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        Some(p) => p,
        None => {
            warn!("task routing: no postgres pool available for capability routing");
            return try_cloud_then_fail(state, &task_type, &req, task_def, &body, &[]).await;
        }
    };

    // Load catalog entries
    let mut catalog_entries: HashMap<String, (String, i32, Value)> = HashMap::new();

    match sqlx::query(
        "SELECT id, name, tier, preferred_workloads FROM fleet_model_catalog",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            for r in rows {
                let id: String = r.get("id");
                let name: String = r.get("name");
                let tier: i32 = r.get("tier");
                let pw: Value = r.get("preferred_workloads");
                catalog_entries.insert(id, (name, tier, pw));
            }
        }
        Err(e) => warn!(error = %e, "task routing: fleet_model_catalog query failed"),
    }

    match sqlx::query(
        "SELECT id, display_name, COALESCE((metadata->>'tier')::int, 2) as tier, metadata->>'preferred_workloads' as pw FROM model_catalog WHERE metadata->>'preferred_workloads' IS NOT NULL"
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            for r in rows {
                let id: String = r.get("id");
                if catalog_entries.contains_key(&id) {
                    continue;
                }
                let name: String = r.get("display_name");
                let tier: i32 = r.get("tier");
                let pw_raw: Option<String> = r.get("pw");
                let pw = pw_raw
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_else(|| Value::Array(vec![]));
                catalog_entries.insert(id, (name, tier, pw));
            }
        }
        Err(e) => warn!(error = %e, "task routing: model_catalog query failed"),
    }

    let available_caps = collect_available_capabilities(&catalog_entries);

    // Fetch live Pulse servers
    let live_servers = match state.pulse_router.as_ref() {
        Some(router) => router.list_servers().await.unwrap_or_default(),
        None => Vec::new(),
    };

    let task_caps: Vec<&str> = task_def.capabilities.to_vec();

    // Match and score candidates
    let mut candidates: Vec<(i32, i32, f64, String, String, String)> = Vec::new();

    for s in &live_servers {
        let _computer = s.get("computer").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let endpoint_raw = s
            .get("endpoint_raw")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let primary_ip = s
            .get("primary_ip")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let raw_model_id = s
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let healthy = s.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
        if !healthy {
            continue;
        }

        let normalized_server_id = crate::llm_routing::normalize_model_id(&raw_model_id);

        // Find matching catalog entry by normalized model id
        let catalog_match = catalog_entries.iter().find(|(cat_id, _)| {
            crate::llm_routing::normalize_model_id(cat_id) == normalized_server_id
        }).map(|(_, v)| v);

        let (name, tier, pw) = match catalog_match {
            Some((n, t, p)) => (n.clone(), *t, p.clone()),
            None => {
                // Fallback: substring match on normalized ids
                let fallback = catalog_entries.iter().find(|(cat_id, _)| {
                    let cat_norm = crate::llm_routing::normalize_model_id(cat_id);
                    normalized_server_id.contains(&cat_norm)
                        || cat_norm.contains(&normalized_server_id)
                }).map(|(_, v)| v);
                match fallback {
                    Some((n, t, p)) => (n.clone(), *t, p.clone()),
                    None => (raw_model_id.clone(), 2, Value::Array(vec![])),
                }
            }
        };

        // Check if model has ANY of the task's capabilities
        let has_any_cap = pw.as_array()
            .map(|arr| {
                arr.iter().any(|v| {
                    v.as_str()
                        .map(|cap| task_caps.iter().any(|tc| tc.eq_ignore_ascii_case(cap)))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if !has_any_cap {
            continue;
        }

        let qd = s
            .get("queue_depth")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        let tps = s
            .get("tokens_per_sec_last_min")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let endpoint = rewrite_endpoint(&endpoint_raw, &primary_ip);
        candidates.push((tier, qd, tps, raw_model_id, name, endpoint));
    }

    // Sort: tier asc, queue asc, tps desc
    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
    });

    if let Some((_, _qd, _tps, model_id, _name, endpoint)) = candidates.first() {
        body["model"] = json!(model_id);
        body["stream"] = json!(false); // downgrade streaming for MVP passthrough

        crate::llm_routing::apply_qwen3_max_tokens_floor(&mut body, model_id);

        let url = if endpoint.contains("/chat/completions") {
            endpoint.clone()
        } else {
            format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'))
        };

        info!(
            task = %task_type,
            model = %model_id,
            endpoint = %url,
            "task routed via capability matching"
        );

        match state.http_client.post(&url).json(&body).send().await {
            Ok(upstream) => {
                let status = upstream.status();
                let bytes = upstream.bytes().await.unwrap_or_default();
                return Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(bytes))
                    .map_err(|e| {
                        (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({
                                "error": {
                                    "message": e.to_string(),
                                    "type": "upstream_error",
                                }
                            })),
                        )
                    });
            }
            Err(err) => {
                warn!(
                    task = %task_type,
                    model = %model_id,
                    %err,
                    "capability-routed upstream request failed; falling back to cloud"
                );
            }
        }
    }

    // ── 3c / 3d. Cloud fallback or final 503 ─────────────────────────────────
    try_cloud_then_fail(state, &task_type, &req, task_def, &body, &available_caps).await
}

async fn try_cloud_then_fail(
    state: Arc<GatewayState>,
    task_type: &str,
    req: &TaskRequest,
    task_def: &TaskDef,
    body: &Value,
    available_caps: &[String],
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    if let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        let model_id = req.model.as_deref().unwrap_or("gpt-4o-mini");
        if let Some(result) = crate::cloud_llm::try_route_to_cloud(pool, model_id, body, None).await
        {
            match result {
                Ok(resp) => {
                    info!(task = %task_type, model = %model_id, "task routed to cloud fallback");
                    return Ok(resp);
                }
                Err(resp) => return Ok(resp),
            }
        }
    }

    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "message": "no healthy fleet endpoint matches the required capabilities and no cloud fallback is available",
                "type": "backend_unavailable",
                "task": task_type,
                "required_capabilities": task_def.capabilities,
                "available_capabilities": available_caps,
            }
        })),
    ))
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Image generation handler
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn handle_image_generation(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<ImageGenerationRequest>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    let mut message = String::from(
        "Image generation is not available on the fleet yet.\n\n\
         Options: (1) Deploy Stable Diffusion/FLUX on a fleet node and register it with \
         `image_generation` capability, or (2) Use a cloud provider directly.",
    );

    if let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        let has_provider: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                SELECT 1 FROM cloud_llm_providers WHERE enabled = true \
                AND (model_prefix ILIKE '%dall%' OR model_prefix ILIKE '%image%' OR model_prefix ILIKE '%flux%')\
            )",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(false);

        if has_provider {
            message.push_str(
                "\n\nA cloud provider for image generation appears to be configured. \
                 Try calling it directly with the appropriate model prefix.",
            );
        }
    }

    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": message,
                "type": "not_implemented",
                "prompt": req.prompt,
            }
        })),
    ))
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Audio transcription handler
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn handle_audio_transcription(
    State(state): State<Arc<GatewayState>>,
    Json(_req): Json<AudioTranscriptionRequest>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    let mut message = String::from(
        "Audio transcription is not available on the fleet yet.\n\n\
         Options: (1) Deploy Whisper on a fleet node and register it with \
         `audio_transcription` capability, or (2) Use a cloud STT provider directly.",
    );

    if let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        let has_provider: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                SELECT 1 FROM cloud_llm_providers WHERE enabled = true \
                AND (model_prefix ILIKE '%whisper%' OR model_prefix ILIKE '%audio%' OR model_prefix ILIKE '%speech%')\
            )",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(false);

        if has_provider {
            message.push_str(
                "\n\nA cloud provider for audio transcription appears to be configured. \
                 Try calling it directly with the appropriate model prefix.",
            );
        }
    }

    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": message,
                "type": "not_implemented",
            }
        })),
    ))
}
