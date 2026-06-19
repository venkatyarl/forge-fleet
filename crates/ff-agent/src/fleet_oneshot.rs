//! Stateless one-shot dispatch to a fleet LLM endpoint.
//!
//! The reusable "prompt → text via a fleet model" primitive — no sub-agent slot
//! claim, no work_outputs persistence (that's `agent_coordinator::dispatch_task`),
//! no MCP JSON shape (that's `ff-mcp::handlers::fleet_run`). Just: pick a healthy
//! deployment from the live router, POST an OpenAI-shape chat completion, return
//! the assistant text plus the endpoint/worker/model that served it (so callers
//! can attribute the turn in `ff_interactions`).
//!
//! Council verdict 2026-06-19 (codex decisive): put the shared primitive in
//! ff-agent (the right dependency direction — ff-terminal & ff-mcp both depend on
//! it) rather than forking an inline POST or making ff-terminal depend on ff-mcp.
//! First caller is `ff council --members local:<model>`; `fleet_run` can migrate
//! onto this later.

use std::time::Duration;

use anyhow::{Result, anyhow};
use ff_db::queries::{RouteCandidate, RouteFilter, pg_route_deployments};
use serde_json::{Value, json};
use sqlx::PgPool;

/// The outcome of a one-shot fleet dispatch — the text plus who served it.
#[derive(Debug, Clone)]
pub struct FleetOneshot {
    pub text: String,
    /// Base endpoint that served the call (e.g. `http://192.168.5.103:55000`).
    pub endpoint: String,
    pub worker_name: String,
    /// The catalog model name that answered (best-effort).
    pub model: String,
    pub latency_ms: u128,
}

/// Dispatch `prompt` to one healthy fleet deployment and return its answer.
///
/// `model_hint` (e.g. `qwen36-35b` from a `local:qwen36-35b` council member)
/// biases candidate selection toward a deployment whose catalog name contains it;
/// when absent or unmatched we take the best-scored healthy candidate. Routing is
/// DB-driven (`pg_route_deployments`) so it always reflects live deployments.
pub async fn fleet_oneshot(
    pool: &PgPool,
    prompt: &str,
    model_hint: Option<&str>,
    timeout: Option<Duration>,
) -> Result<FleetOneshot> {
    let filter = RouteFilter {
        workload: None,
        require_tool_calling: false,
        min_ctx: None,
        exclude_hosts: Vec::new(),
        // Only dispatch to deployments whose health is fresh — never a wedged host
        // lingering as 'healthy' with a stale heartbeat (the priya-wedge class).
        max_health_age_sec: Some(180),
        prefer_least_loaded: true,
        // With a model hint, widen the candidate set so the match isn't truncated:
        // the best-scored top-8 may not include the requested model (e.g. a lower-
        // tier coder deployment), and we'd silently fall back. No hint → top-8.
        limit: if model_hint.is_some() { 64 } else { 8 },
    };
    let candidates = pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow!("route deployments: {e}"))?;
    if candidates.is_empty() {
        return Err(anyhow!(
            "no healthy fleet deployment to serve a local council member"
        ));
    }
    let cand = pick_candidate(&candidates, model_hint);
    let worker_name = cand.worker_name.clone();
    let endpoint = cand.endpoint.clone();
    let model = cand
        .catalog_name
        .clone()
        .or_else(|| model_hint.map(|s| s.to_string()))
        .unwrap_or_else(|| "local".to_string());

    let url = ff_core::url::normalize_chat_completions_url(&endpoint);
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });
    let client = reqwest::Client::builder()
        .timeout(timeout.unwrap_or(Duration::from_secs(180)))
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))?;

    let start = std::time::Instant::now();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("decode response from {worker_name}: {e}"))?;
    if !status.is_success() {
        return Err(anyhow!(
            "{worker_name} ({model}) returned HTTP {status}: {}",
            payload.to_string().chars().take(400).collect::<String>()
        ));
    }
    let text = extract_completion_text(&payload)
        .map(|t| strip_think_block(&t))
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow!("{worker_name} ({model}) returned an empty completion"))?;

    Ok(FleetOneshot {
        text,
        endpoint,
        worker_name,
        model,
        latency_ms: start.elapsed().as_millis(),
    })
}

/// Choose a candidate: the first whose catalog id OR name contains `model_hint`
/// (case-insensitive), else the best-scored one (candidates are pre-sorted).
/// Matching both fields means `local:qwen3-coder` hits a deployment whose
/// catalog_id is `qwen3-coder-30b` even when the display name is spelled
/// differently.
fn pick_candidate<'a>(
    candidates: &'a [RouteCandidate],
    model_hint: Option<&str>,
) -> &'a RouteCandidate {
    if let Some(hint) = model_hint
        .map(|h| h.to_lowercase())
        .filter(|h| !h.is_empty())
        && let Some(c) = candidates.iter().find(|c| {
            let hay = |s: &Option<String>| {
                s.as_deref()
                    .map(|v| v.to_lowercase().contains(&hint))
                    .unwrap_or(false)
            };
            hay(&c.catalog_id) || hay(&c.catalog_name)
        })
    {
        return c;
    }
    &candidates[0]
}

/// Pull the assistant text out of an OpenAI-shape chat-completion payload,
/// tolerating both `message.content` and the legacy `text` field.
fn extract_completion_text(payload: &Value) -> Option<String> {
    let choice = payload.get("choices")?.as_array()?.first()?;
    if let Some(content) = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        && !content.is_empty()
    {
        return Some(content.to_string());
    }
    choice
        .get("text")
        .and_then(|t| t.as_str())
        .map(String::from)
}

/// Strip a leading `<think>…</think>` reasoning block some local models emit so
/// the council sees only the answer.
fn strip_think_block(s: &str) -> String {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("<think>")
        && let Some(end) = rest.find("</think>")
    {
        return rest[end + "</think>".len()..].trim().to_string();
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_message_then_text() {
        let p = json!({"choices":[{"message":{"content":"hello"}}]});
        assert_eq!(extract_completion_text(&p).as_deref(), Some("hello"));
        let p = json!({"choices":[{"text":"legacy"}]});
        assert_eq!(extract_completion_text(&p).as_deref(), Some("legacy"));
        assert_eq!(extract_completion_text(&json!({})), None);
    }

    #[test]
    fn strips_think_block() {
        assert_eq!(
            strip_think_block("<think>reasoning</think>  answer"),
            "answer"
        );
        assert_eq!(strip_think_block("plain"), "plain");
    }
}
