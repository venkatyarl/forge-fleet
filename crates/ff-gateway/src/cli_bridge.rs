//! Layer 3 of the multi-LLM CLI integration: a local OpenAI-compatible
//! HTTP bridge that spawns a vendor CLI per request.
//!
//! Five bridges (one per provider) listen on fixed ports
//! 51100/51101/51102/51103/51104 on every fleet member. Each speaks
//! `POST /v1/chat/completions` (OpenAI shape) and translates to a vendor
//! CLI subprocess invocation via `ff_agent::cli_executor`. With ~14
//! members × 5 CLIs, the fleet exposes ~70 concurrent CLI-agent slots.
//!
//! Routing into the bridge: any model-name with prefix `claude-cli-`,
//! `codex-cli-`, `gemini-cli-`, `kimi-cli-`, `grok-cli-` matches a
//! `local_bridge` row in `cloud_llm_providers` (V53) whose `base_url`
//! is `http://127.0.0.1:5110X`. The existing
//! `cloud_llm.rs::try_route_to_cloud` then issues an OpenAI-style POST
//! to that URL. The bridge handles it locally — no api_key or oauth
//! token needed (auth_kind=`local_bridge`).
//!
//! Per-port startup is gated on the binary actually being present on
//! `$PATH`. Members without `gemini` simply don't open 51103 — the
//! cloud_llm_providers row will then return a connection-refused
//! error, which is the right signal that this member can't serve that
//! backend.

use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use ff_agent::cli_executor::{BACKENDS, CliBackend, execute_cli_local_in_dir};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// One spawn slot per backend. Returns the JoinHandles so the daemon
/// can keep them alive for the process lifetime. Skips backends whose
/// binary isn't on PATH (no error, just a debug log).
pub fn spawn_all_bridges() -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for backend in BACKENDS.iter() {
        // Explicit per-backend port (matches the cli_backends DB seed); never
        // array-position-derived — that silently cross-wired kimi/gemini when
        // the array order drifted from the seed (deep review conflict #8).
        let port = backend.port;
        if ff_agent::cli_executor::which_on_path(backend.binary).is_none() {
            tracing::debug!(
                backend = backend.name,
                port,
                "cli_bridge: skipping (no `{}` on PATH)",
                backend.binary
            );
            continue;
        }
        info!(backend = backend.name, port, "spawning cli_bridge");
        handles.push(tokio::spawn(run_bridge(*backend, port)));
    }
    handles
}

/// One axum server bound to `127.0.0.1:<port>` that translates
/// `/v1/chat/completions` to a CLI invocation.
async fn run_bridge(backend: CliBackend, port: u16) {
    let app = Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .with_state(backend);
    let addr = ff_agent::http_auth::bind_addr(port)
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                warn!(backend = backend.name, port, error = %e, "cli_bridge stopped");
            }
        }
        Err(e) => warn!(backend = backend.name, port, error = %e, "cli_bridge failed to bind"),
    }
}

/// Minimal `OpenAI /v1/chat/completions` request shape — just enough to
/// extract the prompt. Extra fields (temperature, max_tokens, …) are
/// ignored; the vendor CLI has its own defaults.
#[derive(Debug, Deserialize)]
struct ChatCompletionsRequest {
    model: Option<String>,
    messages: Vec<ChatMessage>,
    /// Optional pass-through args appended to the CLI invocation
    /// (non-standard; only ff clients use it).
    #[serde(default)]
    backend_args: Vec<String>,
    work_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    content: Value,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsResponse {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Debug, Serialize)]
struct Choice {
    index: u32,
    message: ChoiceMessage,
    finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
struct ChoiceMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

async fn handle_chat_completions(
    State(backend): State<CliBackend>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    if !peer.ip().is_loopback() {
        let secret = match ff_agent::http_auth::control_plane_secret() {
            Ok(secret) => secret,
            Err(error) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": error})),
                )
                    .into_response();
            }
        };
        if let Err(error) =
            ff_agent::http_auth::authorize(&secret, "POST", "/v1/chat/completions", &headers, &body)
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": error})),
            )
                .into_response();
        }
    }
    let req: ChatCompletionsRequest = match serde_json::from_str(&body) {
        Ok(req) => req,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid json: {error}")})),
            )
                .into_response();
        }
    };
    // Build the prompt by concatenating user messages. System messages
    // are prepended as a "[system]" block; this is the simplest
    // translation that preserves intent across vendor CLIs whose
    // prompt-from-stdin shapes vary.
    let mut prompt = String::new();
    for m in &req.messages {
        let role = m.role.as_str();
        let text = match &m.content {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.get("text").and_then(Value::as_str).map(str::to_string))
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        };
        match role {
            "system" => prompt.push_str(&format!("[system]\n{text}\n\n")),
            "user" => prompt.push_str(&text),
            "assistant" => prompt.push_str(&format!("\n[previous-assistant]\n{text}\n\n")),
            _ => {}
        }
    }
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"empty messages"})),
        )
            .into_response();
    }

    let timeout = Some(Duration::from_secs(10 * 60));
    let work_dir = req.work_dir.as_deref().map(std::path::Path::new);
    let result =
        execute_cli_local_in_dir(backend.name, &prompt, &req.backend_args, work_dir, timeout).await;

    match result {
        Ok(r) if r.exit_code == 0 => {
            let prompt_tokens = (prompt.chars().count() / 4) as u32;
            let completion_tokens = (r.stdout.chars().count() / 4) as u32;
            let resp = ChatCompletionsResponse {
                id: format!("ffbridge-{}", chrono::Utc::now().timestamp_millis()),
                object: "chat.completion",
                created: chrono::Utc::now().timestamp(),
                model: req.model.unwrap_or_else(|| format!("{}-cli", backend.name)),
                choices: vec![Choice {
                    index: 0,
                    message: ChoiceMessage {
                        role: "assistant",
                        content: r.stdout,
                    },
                    finish_reason: "stop",
                }],
                usage: Usage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens + completion_tokens,
                },
            };
            (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response()
        }
        Ok(r) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": {
                    "type": "cli_nonzero_exit",
                    "code": r.exit_code,
                    "stderr": r.stderr.chars().take(2000).collect::<String>(),
                }
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": {"type":"cli_spawn_failed","message": e.to_string()}
            })),
        )
            .into_response(),
    }
}
