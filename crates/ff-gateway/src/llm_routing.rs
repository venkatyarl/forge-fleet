//! Pulse-v2-backed LLM request routing.
//!
//! This module replaces the older `inference_router` logic for the new
//! `/fleet/chat/completions` (and optionally `/v2/chat/completions`) endpoints.
//! Instead of consulting the ff-api `BackendRegistry`, it reads live LLM-server
//! state directly from Redis via [`ff_pulse::reader::PulseReader`], so any
//! fleet node that is currently beating with an active+healthy LLM server is
//! immediately routable — no explicit backend configuration required.
//!
//! Key differences from `proxy_chat_completions`:
//! - Source of truth is Redis Pulse beats (ephemeral fleet state), not a
//!   statically configured registry.
//! - Model-name matching is **case-insensitive prefix** match against each
//!   server's reported `model.id`. That way a request for `Qwen3-Coder-30B-A3B`
//!   can land on a server whose model id is `Qwen3-Coder-30B-A3B-Q4_K_M`
//!   (llama.cpp) or `qwen3-coder-30b-a3b:latest` (ollama).
//! - Candidate selection breaks ties by lowest `queue_depth`, then highest
//!   `tokens_per_sec_last_min`.
//! - When no candidate is found we report the list of loaded models fleet-wide
//!   so the caller sees what they *could* have asked for.

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use thiserror::Error;

use ff_pulse::beat_v2::{LlmServer, PulseBeatV2};
use ff_pulse::reader::{PulseError, PulseReader};

/// Errors returned by [`route_completion`].
#[derive(Debug, Error)]
pub enum LlmRoutingError {
    #[error("pulse: {0}")]
    Pulse(#[from] PulseError),

    #[error("missing `model` field on request")]
    MissingModel,

    /// No active+healthy LLM server in the fleet matches the requested model.
    #[error("no server has model '{requested}' loaded")]
    NoMatch {
        requested: String,
        available: Vec<String>,
    },

    #[error("upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),

    #[error("upstream timed out after {0:?}")]
    Timeout(Duration),
}

/// A resolved routing decision — which server was picked for this request.
#[derive(Debug, Clone)]
pub struct RoutedServer {
    pub computer: String,
    pub endpoint: String,
    pub runtime: String,
    pub model_id: String,
    pub queue_depth: i32,
}

/// Pulse-backed LLM router. Wraps a [`PulseReader`] and a reusable reqwest
/// client so upstream connections pool across many requests.
#[derive(Clone)]
pub struct PulseLlmRouter {
    reader: std::sync::Arc<PulseReader>,
    http: Client,
    upstream_timeout: Duration,
}

impl PulseLlmRouter {
    /// Construct a new router pointed at `redis_url`.
    ///
    /// The Redis URL usually comes from `$FORGEFLEET_REDIS_URL`; callers
    /// should respect that convention.
    pub fn new(redis_url: &str) -> Result<Self, LlmRoutingError> {
        let reader = PulseReader::new(redis_url)?;
        let http = Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Ok(Self {
            reader: std::sync::Arc::new(reader),
            http,
            upstream_timeout: Duration::from_secs(120),
        })
    }

    /// Collect every active+healthy LLM server paired with the beat it
    /// came from, so callers can look up the node's primary IP for
    /// cross-host routing.
    async fn collect_active(&self) -> Result<Vec<(PulseBeatV2, LlmServer)>, LlmRoutingError> {
        let beats = self.reader.all_beats().await?;
        let mut out = Vec::new();
        for b in beats {
            if b.going_offline {
                continue;
            }
            for s in &b.llm_servers {
                if s.status == "active" && s.is_healthy {
                    out.push((b.clone(), s.clone()));
                }
            }
        }
        Ok(out)
    }

    /// Return every active+healthy LLM server in the fleet, in a shape
    /// convenient for the `/v1/fleet/servers` debug endpoint.
    pub async fn list_servers(&self) -> Result<Vec<Value>, LlmRoutingError> {
        let raw = self.collect_active().await?;
        Ok(raw
            .into_iter()
            .map(|(beat, s)| {
                let routed_endpoint = rewrite_endpoint(&s.endpoint, &beat.network.primary_ip);
                json!({
                    "computer": beat.computer_name,
                    "endpoint": routed_endpoint,
                    "endpoint_raw": s.endpoint,
                    "primary_ip": beat.network.primary_ip,
                    "runtime": s.runtime,
                    "model": s.model.id,
                    "healthy": s.is_healthy,
                    "status": s.status,
                    "queue_depth": s.queue_depth,
                    "tokens_per_sec_last_min": s.tokens_per_sec_last_min,
                })
            })
            .collect())
    }

    /// Pick the best candidate for `requested_model` using:
    ///   1. Normalize both the requested name and each server's `model.id`.
    ///      Normalization strips Ollama-style tags (`foo:14b` → `foo`),
    ///      `.gguf` extensions, common quantization suffixes
    ///      (`-q4_k_m`, `-q8_0`, `-bf16`, etc.), and folds underscores to
    ///      dashes, lowercased.
    ///   2. Prefer exact post-normalization match.
    ///   3. Otherwise accept prefix match in either direction.
    ///   4. Tie-break by lowest `queue_depth`, then highest
    ///      `tokens_per_sec_last_min`.
    ///   5. Exact matches always rank ahead of prefix matches.
    ///
    /// Returns `(computer_name, primary_ip, LlmServer)` when found.
    pub async fn pick_server(
        &self,
        requested_model: &str,
    ) -> Result<Option<(String, String, LlmServer)>, LlmRoutingError> {
        let requested_raw = requested_model.to_ascii_lowercase();
        let requested_norm = normalize_model_id(requested_model);
        let all = self.collect_active().await?;

        // Match rank, lower = better:
        //   0 = raw case-insensitive exact (preserves Ollama tag like `:14b`)
        //   1 = normalized exact (tag/quant stripped both sides)
        //   2 = normalized prefix match in either direction
        let mut candidates: Vec<(u8, PulseBeatV2, LlmServer)> = all
            .into_iter()
            .filter_map(|(b, s)| {
                let id_raw = s.model.id.to_ascii_lowercase();
                let id_norm = normalize_model_id(&s.model.id);
                if id_raw == requested_raw {
                    Some((0u8, b, s))
                } else if id_norm == requested_norm {
                    Some((1u8, b, s))
                } else if id_norm.starts_with(&requested_norm)
                    || requested_norm.starts_with(&id_norm)
                {
                    Some((2u8, b, s))
                } else {
                    None
                }
            })
            .collect();

        // Primary: best match rank. Secondary: lowest queue_depth.
        // Tertiary: highest tokens/sec_last_min.
        candidates.sort_by(|(a_rank, _, a), (b_rank, _, b)| {
            a_rank
                .cmp(b_rank)
                .then_with(|| a.queue_depth.cmp(&b.queue_depth))
                .then_with(|| {
                    b.tokens_per_sec_last_min
                        .partial_cmp(&a.tokens_per_sec_last_min)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        Ok(candidates
            .into_iter()
            .next()
            .map(|(_, b, s)| (b.computer_name, b.network.primary_ip, s)))
    }

    /// Full end-to-end: extract `model` from the body, pick a server, and
    /// proxy the JSON request to that server's `/v1/chat/completions`.
    ///
    /// Streaming is NOT supported in v1 — if the request has `"stream": true`,
    /// it is downgraded to non-streaming transparently.
    pub async fn route_completion(&self, mut body: Value) -> Result<Value, LlmRoutingError> {
        let requested_model = body
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or(LlmRoutingError::MissingModel)?;

        // Downgrade streaming for v1.
        if body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
            body["stream"] = Value::Bool(false);
        }

        let Some((computer, primary_ip, server)) = self.pick_server(&requested_model).await?
        else {
            // Gather available model ids fleet-wide for a helpful error.
            let all = self.reader.list_llm_servers().await?;
            let available: Vec<String> =
                all.into_iter().map(|(_, s)| s.model.id).collect();
            return Err(LlmRoutingError::NoMatch {
                requested: requested_model,
                available,
            });
        };

        // Beats report endpoints as `http://127.0.0.1:PORT` (because the LLM
        // probe runs on the same host as the inference server). Rewrite the
        // loopback host to the node's primary IP so the gateway can reach it
        // across the LAN.
        let rewritten_endpoint = rewrite_endpoint(&server.endpoint, &primary_ip);

        let routed = RoutedServer {
            computer,
            endpoint: rewritten_endpoint.clone(),
            runtime: server.runtime.clone(),
            model_id: server.model.id.clone(),
            queue_depth: server.queue_depth,
        };

        // Build the upstream URL. If the endpoint already ends in
        // `/v1/chat/completions` (or similar), use it as-is; otherwise append.
        let url = if rewritten_endpoint.contains("/chat/completions") {
            rewritten_endpoint.clone()
        } else {
            let base = rewritten_endpoint.trim_end_matches('/');
            // Ollama uses /v1/chat/completions too (it has an OpenAI shim).
            format!("{base}/v1/chat/completions")
        };

        tracing::debug!(
            computer = %routed.computer,
            endpoint = %routed.endpoint,
            runtime = %routed.runtime,
            model_id = %routed.model_id,
            queue_depth = routed.queue_depth,
            "pulse: proxying chat completion"
        );

        let fut = self.http.post(&url).json(&body).send();
        let resp = tokio::time::timeout(self.upstream_timeout, fut)
            .await
            .map_err(|_| LlmRoutingError::Timeout(self.upstream_timeout))??;

        let status = resp.status();
        let mut v: Value = resp.json().await?;

        // Decorate with routing info for diagnostics; put it under an internal
        // key so it does not collide with OpenAI's schema.
        if v.is_object() {
            v["_forgefleet_route"] = json!({
                "computer": routed.computer,
                "endpoint": routed.endpoint,
                "runtime": routed.runtime,
                "upstream_status": status.as_u16(),
            });
        }
        Ok(v)
    }
}

/// Normalize a model identifier so heterogeneous fleet-reported model IDs
/// can be matched against user-supplied model names.
///
/// Handles (at least):
/// - Ollama tags:  `qwen2.5-coder:14b`        → `qwen2.5-coder`
/// - GGUF files:   `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf`
///                                             → `qwen3-coder-30b-a3b-instruct`
/// - Mixed case + underscore separators        → lowercased, dashed
/// - Common llama.cpp/HF quantization suffixes are stripped so a bare
///   family name (`qwen3-coder-30b-a3b`) prefix-matches the richer id.
pub(crate) fn normalize_model_id(raw: &str) -> String {
    // Lowercase first.
    let mut s = raw.to_ascii_lowercase();

    // Path-component: keep only the final segment (for HF repo-style ids
    // like `Qwen/Qwen3-Coder-30B-A3B`).
    if let Some(idx) = s.rfind('/') {
        s = s[idx + 1..].to_string();
    }

    // Drop anything after a colon (Ollama tag — `:14b`, `:latest`).
    if let Some(idx) = s.find(':') {
        s.truncate(idx);
    }

    // Strip trailing `.gguf` / `.bin` / `.safetensors` extension.
    for ext in [".gguf", ".bin", ".safetensors"] {
        if s.ends_with(ext) {
            s.truncate(s.len() - ext.len());
            break;
        }
    }

    // Normalize separators: underscores → dashes, collapse runs of dashes.
    s = s.replace('_', "-");
    while s.contains("--") {
        s = s.replace("--", "-");
    }

    // Strip common quantization / precision suffixes if trailing.
    // Order matters: longer suffixes first so we don't leave a stray dash.
    let quant_suffixes: &[&str] = &[
        "-q2-k", "-q3-k-s", "-q3-k-m", "-q3-k-l",
        "-q4-0", "-q4-1", "-q4-k-s", "-q4-k-m",
        "-q5-0", "-q5-1", "-q5-k-s", "-q5-k-m",
        "-q6-k", "-q8-0",
        "-bf16", "-fp16", "-fp8", "-f16", "-f32",
        "-int8", "-int4",
        "-awq", "-gptq",
    ];
    // Strip repeatedly — a filename may carry more than one precision tag.
    loop {
        let mut changed = false;
        for sfx in quant_suffixes {
            if s.ends_with(sfx) {
                s.truncate(s.len() - sfx.len());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Trim leading/trailing dashes left over from stripping.
    s.trim_matches('-').to_string()
}

/// Replace `127.0.0.1` / `localhost` / `0.0.0.0` in an endpoint URL with
/// the node's reachable `primary_ip`. If `primary_ip` is empty, returns
/// the original endpoint unchanged.
fn rewrite_endpoint(endpoint: &str, primary_ip: &str) -> String {
    if primary_ip.is_empty() {
        return endpoint.to_string();
    }
    let loopbacks = ["127.0.0.1", "localhost", "0.0.0.0"];
    for lb in loopbacks {
        // Look for `://loopback` (scheme-relative) to avoid accidentally
        // rewriting path components that happen to contain the string.
        let needle = format!("://{lb}");
        if let Some(idx) = endpoint.find(&needle) {
            let before = &endpoint[..idx + 3]; // include "://"
            let after = &endpoint[idx + needle.len()..];
            return format!("{before}{primary_ip}{after}");
        }
    }
    endpoint.to_string()
}

/// Shape an [`LlmRoutingError`] into a (status, json) tuple for axum handlers.
pub fn error_to_response(err: LlmRoutingError) -> (u16, Value) {
    match err {
        LlmRoutingError::MissingModel => (
            400,
            json!({"error": {"message": "missing `model` field", "type": "invalid_request_error"}}),
        ),
        LlmRoutingError::NoMatch { requested, available } => (
            404,
            json!({"error": {
                "message": format!("no server has model '{}' loaded", requested),
                "type": "model_not_loaded",
                "available": available,
            }}),
        ),
        LlmRoutingError::Timeout(d) => (
            504,
            json!({"error": {
                "message": format!("upstream timed out after {}s", d.as_secs()),
                "type": "upstream_timeout",
            }}),
        ),
        LlmRoutingError::Upstream(e) => (
            502,
            json!({"error": {
                "message": format!("upstream request failed: {}", e),
                "type": "upstream_error",
            }}),
        ),
        LlmRoutingError::Pulse(e) => (
            503,
            json!({"error": {
                "message": format!("pulse reader unavailable: {}", e),
                "type": "pulse_unavailable",
            }}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_shapes_no_match() {
        let (code, body) = error_to_response(LlmRoutingError::NoMatch {
            requested: "foo".into(),
            available: vec!["bar".into(), "baz".into()],
        });
        assert_eq!(code, 404);
        assert_eq!(
            body["error"]["message"].as_str().unwrap(),
            "no server has model 'foo' loaded"
        );
        let avail = body["error"]["available"].as_array().unwrap();
        assert_eq!(avail.len(), 2);
    }

    #[test]
    fn error_response_shapes_missing_model() {
        let (code, _body) = error_to_response(LlmRoutingError::MissingModel);
        assert_eq!(code, 400);
    }

    #[test]
    fn rewrite_endpoint_replaces_loopback() {
        assert_eq!(
            rewrite_endpoint("http://127.0.0.1:55000", "192.168.5.102"),
            "http://192.168.5.102:55000"
        );
        assert_eq!(
            rewrite_endpoint("http://localhost:11434/v1", "192.168.5.103"),
            "http://192.168.5.103:11434/v1"
        );
        assert_eq!(
            rewrite_endpoint("http://0.0.0.0:51001", "10.0.0.5"),
            "http://10.0.0.5:51001"
        );
    }

    #[test]
    fn rewrite_endpoint_leaves_other_hosts_alone() {
        assert_eq!(
            rewrite_endpoint("http://192.168.5.100:55000", "192.168.5.102"),
            "http://192.168.5.100:55000"
        );
    }

    #[test]
    fn rewrite_endpoint_empty_primary_ip_noop() {
        assert_eq!(
            rewrite_endpoint("http://127.0.0.1:55000", ""),
            "http://127.0.0.1:55000"
        );
    }

    #[test]
    fn normalize_strips_ollama_tag() {
        assert_eq!(normalize_model_id("qwen2.5-coder:14b"), "qwen2.5-coder");
        assert_eq!(normalize_model_id("qwen2.5-coder:latest"), "qwen2.5-coder");
        assert_eq!(normalize_model_id("Qwen2.5-Coder:14B"), "qwen2.5-coder");
    }

    #[test]
    fn normalize_strips_gguf_and_quant() {
        assert_eq!(
            normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"),
            "qwen3-coder-30b-a3b-instruct"
        );
        assert_eq!(
            normalize_model_id("Qwen2.5-Coder-32B-Instruct-Q8_0.gguf"),
            "qwen2.5-coder-32b-instruct"
        );
    }

    #[test]
    fn normalize_prefix_match_bare_vs_tagged() {
        // Bare name vs ollama-tagged server: both normalize to the same stem.
        let bare = normalize_model_id("qwen2.5-coder");
        let tagged = normalize_model_id("qwen2.5-coder:14b");
        assert_eq!(bare, tagged);
        assert_eq!(bare, "qwen2.5-coder");
    }

    #[test]
    fn normalize_prefix_request_matches_richer_id() {
        // A user asks for `qwen3-coder-30b-a3b`, server has
        // `qwen3-coder-30b-a3b-instruct`. Post-normalize, prefix match holds.
        let requested = normalize_model_id("qwen3-coder-30b-a3b");
        let server = normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf");
        assert!(server.starts_with(&requested));
    }

    #[test]
    fn normalize_handles_hf_repo_path() {
        // HF-style `Owner/Repo` ids — keep last segment.
        assert_eq!(
            normalize_model_id("Qwen/Qwen3-Coder-30B-A3B-Instruct"),
            "qwen3-coder-30b-a3b-instruct"
        );
    }
}
