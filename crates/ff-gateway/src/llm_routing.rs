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
    ///   1. Case-insensitive prefix match on `model.id`.
    ///   2. Lowest queue_depth.
    ///   3. Highest tokens_per_sec_last_min.
    ///
    /// Returns `(computer_name, primary_ip, LlmServer)` when found.
    pub async fn pick_server(
        &self,
        requested_model: &str,
    ) -> Result<Option<(String, String, LlmServer)>, LlmRoutingError> {
        let requested = requested_model.to_ascii_lowercase();
        let all = self.collect_active().await?;

        let mut candidates: Vec<(PulseBeatV2, LlmServer)> = all
            .into_iter()
            .filter(|(_, s)| {
                let id = s.model.id.to_ascii_lowercase();
                // Prefer exact match, otherwise prefix match in either direction
                // (request may be shorter OR longer than the server's id).
                id == requested || id.starts_with(&requested) || requested.starts_with(&id)
            })
            .collect();

        candidates.sort_by(|(_, a), (_, b)| {
            a.queue_depth.cmp(&b.queue_depth).then_with(|| {
                b.tokens_per_sec_last_min
                    .partial_cmp(&a.tokens_per_sec_last_min)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        Ok(candidates
            .into_iter()
            .next()
            .map(|(b, s)| (b.computer_name, b.network.primary_ip, s)))
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
}
