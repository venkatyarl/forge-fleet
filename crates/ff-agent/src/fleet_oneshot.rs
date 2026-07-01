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
    /// Prompt/completion tokens from the response `usage` block (0 when the
    /// server omits it), so callers can attribute the turn's cost in
    /// `ff_interactions` instead of logging 0/0.
    pub tokens_in: i32,
    pub tokens_out: i32,
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
    let all_candidates = pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow!("route deployments: {e}"))?;
    if all_candidates.is_empty() {
        return Err(anyhow!(
            "no healthy fleet deployment to serve a local council member"
        ));
    }
    // Drop deployments with no usable model name (empty catalog_id AND
    // catalog_name). Those are "unknown model" rows — e.g. ace's mlx:55000,
    // which is marked healthy but is NOT a real chat-completions server: sending
    // it `model="local"` makes it try to fetch a HF repo named "local" and
    // return an HTTP error, which masked as "fleet_oneshot round 1" and forced
    // every local codegen dispatch to fall back to slow cloud codex
    // (dogfooded 2026-07-01). Only keep them as a last resort so a fleet with
    // ONLY unknown-model deployments still attempts a call.
    let named: Vec<RouteCandidate> = all_candidates
        .iter()
        .filter(|c| has_model_name(c))
        .cloned()
        .collect();
    let candidates: &[RouteCandidate] = if named.is_empty() {
        &all_candidates
    } else {
        &named
    };
    // Ordered try-list: model-hint match first (if any), then the rest in
    // pg_route_deployments preference order. FAIL OVER across candidates — a
    // single transient transport error (a busy/restarting endpoint, e.g.
    // veronica:55000 mid-load) previously errored the WHOLE call with no retry,
    // blocking every fleet_oneshot caller (decompose / research / council-local)
    // even though other healthy endpoints were available (dogfooded 2026-07-01).
    // pick_candidate applies the model-hint preference (lowercased; matches both
    // catalog_id and catalog_name) — put its choice first, then the rest of the
    // healthy candidates in preference order as fail-over targets.
    let preferred = pick_candidate(candidates, model_hint);
    let mut ordered: Vec<&RouteCandidate> = vec![preferred];
    ordered.extend(candidates.iter().filter(|c| !std::ptr::eq(*c, preferred)));

    let client = reqwest::Client::builder()
        .timeout(timeout.unwrap_or(Duration::from_secs(180)))
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))?;

    let mut last_err: Option<anyhow::Error> = None;
    for cand in ordered {
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
        let start = std::time::Instant::now();

        let attempt: anyhow::Result<FleetOneshot> = async {
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
            let (tokens_in, tokens_out) = usage_tokens_i32(&payload);
            Ok(FleetOneshot {
                text,
                endpoint: endpoint.clone(),
                worker_name: worker_name.clone(),
                model: model.clone(),
                latency_ms: start.elapsed().as_millis(),
                tokens_in,
                tokens_out,
            })
        }
        .await;

        match attempt {
            Ok(ok) => return Ok(ok),
            Err(e) => {
                tracing::warn!(worker = %worker_name, error = %e, "fleet_oneshot: candidate failed — failing over to next");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("all fleet candidates failed")))
}

/// Read `(tokens_in, tokens_out)` from a chat-completion `usage` block, clamped
/// into `i32` for the `ff_interactions` columns. Reuses the canonical
/// `research::parse_completion_usage` walk (no forked JSON parsing); a server
/// that omits `usage`, or absurd values, degrade to `0`/`i32::MAX`. Pure.
fn usage_tokens_i32(payload: &Value) -> (i32, i32) {
    let (pt, ct) = crate::research::parse_completion_usage(payload);
    let clamp = |n: u64| i32::try_from(n).unwrap_or(i32::MAX);
    (clamp(pt), clamp(ct))
}

/// True if the deployment carries a usable model name (non-empty catalog_id or
/// catalog_name). A candidate with neither can't be given a valid `model` value
/// and is often not a real chat server (see the ace mlx:55000 case), so
/// `fleet_oneshot` excludes these from selection except as a last resort. Pure.
fn has_model_name(c: &RouteCandidate) -> bool {
    model_name_present(c.catalog_id.as_deref(), c.catalog_name.as_deref())
}

/// Pure core of [`has_model_name`]: true when either field is non-empty.
fn model_name_present(catalog_id: Option<&str>, catalog_name: Option<&str>) -> bool {
    let present = |s: Option<&str>| s.map(|v| !v.trim().is_empty()).unwrap_or(false);
    present(catalog_id) || present(catalog_name)
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

    // Authored by a fleet model (qwen36 on lily) via `ff offload`, hand-verified,
    // then integrated — dogfooding the fleet for test-gen (grows ff_interactions).
    // Pins the usage→i32 clamp that feeds council token attribution.
    #[test]
    fn usage_tokens_i32_reads_usage() {
        assert_eq!(
            usage_tokens_i32(&json!({"usage":{"prompt_tokens":123,"completion_tokens":45}})),
            (123, 45)
        );
        assert_eq!(usage_tokens_i32(&json!({})), (0, 0));
        assert_eq!(
            usage_tokens_i32(
                &json!({"usage":{"prompt_tokens":5000000000u64,"completion_tokens":0}})
            ),
            (i32::MAX, 0)
        );
    }

    #[test]
    fn strips_think_block() {
        assert_eq!(
            strip_think_block("<think>reasoning</think>  answer"),
            "answer"
        );
        assert_eq!(strip_think_block("plain"), "plain");
    }

    #[test]
    fn model_name_present_excludes_unknown_deployments() {
        // A named coder deployment passes.
        assert!(model_name_present(Some("qwen3-coder-30b"), None));
        assert!(model_name_present(None, Some("Qwen3 Coder")));
        // ace's mlx:55000 "unknown model" — empty/whitespace/None both ways — is
        // excluded so fleet_oneshot never routes local codegen to a non-chat
        // endpoint that returns HTTP errors (the Lane-1 root cause).
        assert!(!model_name_present(None, None));
        assert!(!model_name_present(Some(""), Some("  ")));
        assert!(!model_name_present(Some("   "), None));
    }
}
