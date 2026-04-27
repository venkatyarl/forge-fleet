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

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use ff_agent::cli_executor::{BACKENDS, CliBackend, execute_cli};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// One spawn slot per backend. Returns the JoinHandles so the daemon
/// can keep them alive for the process lifetime. Skips backends whose
/// binary isn't on PATH (no error, just a debug log).
pub fn spawn_all_bridges() -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for (idx, backend) in BACKENDS.iter().enumerate() {
        let port = 51100 + idx as u16;
        if !is_binary_on_path(backend.binary) {
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
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            if let Err(e) = axum::serve(listener, app).await {
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
    Json(req): Json<ChatCompletionsRequest>,
) -> impl IntoResponse {
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
    let result = execute_cli(backend.name, &prompt, &req.backend_args, timeout).await;

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

fn is_binary_on_path(bin: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        if dir.join(bin).is_file() {
            return true;
        }
    }
    false
}
