//! Cloud-LLM router.
//!
//! Gateway-side counterpart to `ff_agent::cloud_llm_registry`. When a
//! `/v1/chat/completions` request arrives with a `model` field whose
//! prefix matches a row in `cloud_llm_providers` (schema V26), this
//! module forwards the request off-fleet to the provider's public API
//! (OpenAI / Anthropic / Moonshot / Google).
//!
//! Entry point [`try_route_to_cloud`] returns:
//!   - `Some(Ok(response))`  — matched + succeeded (or matched + HTTP error)
//!   - `Some(Err(response))` — matched but a local failure (e.g. missing key)
//!   - `None`                — no cloud match; caller falls through to Pulse.
//!
//! API keys come from `fleet_secrets`. They are never logged — we emit
//! `<redacted>` in any diagnostic.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Response, StatusCode};
use reqwest::Client;
use serde_json::{Value, json};
use sqlx::PgPool;

use ff_agent::cloud_llm_registry::{self, Provider};
use ff_db::pg_get_secret;

const CLOUD_TIMEOUT: Duration = Duration::from_secs(120);

/// Max attempts when the upstream returns 429. Counts the initial call,
/// so 3 = one call + two retries.
const MAX_429_ATTEMPTS: usize = 3;

/// Cap on `Retry-After` honoring. Vendors occasionally return absurdly
/// long values; clamp to keep the request within `CLOUD_TIMEOUT` budget.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Default backoff when 429 came back without a `Retry-After` header.
const DEFAULT_429_BACKOFF: Duration = Duration::from_secs(2);

fn build_client() -> Client {
    Client::builder()
        .timeout(CLOUD_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Send a request with retry-on-429. Honors `Retry-After` (capped at
/// [`MAX_RETRY_AFTER`]) when present; falls back to
/// [`DEFAULT_429_BACKOFF`] otherwise. Non-429 responses (success or
/// other errors) return immediately. Builder must be cloneable, so the
/// JSON body is set on each attempt's clone.
async fn send_with_429_retry(
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        // try_clone returns None if the body is a stream. Our cloud
        // calls all use JSON bodies, so this should always succeed.
        let req = match builder.try_clone() {
            Some(r) => r,
            None => return builder.send().await,
        };
        let resp = req.send().await?;
        if resp.status().as_u16() != 429 || attempt >= MAX_429_ATTEMPTS {
            return Ok(resp);
        }
        let wait = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .map(|d| d.min(MAX_RETRY_AFTER))
            .unwrap_or(DEFAULT_429_BACKOFF);
        tracing::warn!(
            attempt,
            wait_secs = wait.as_secs(),
            "cloud_llm: upstream 429, backing off"
        );
        tokio::time::sleep(wait).await;
    }
}

fn error_response(status: StatusCode, message: impl Into<String>, kind: &str) -> Response<Body> {
    let body = json!({ "error": { "message": message.into(), "type": kind } });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn ok_response(mut body: Value, provider: &Provider, status: StatusCode) -> Response<Body> {
    if body.is_object() {
        body["_forgefleet_route"] = json!({
            "cloud_provider": provider.id,
            "request_format": provider.request_format,
            "upstream_status": status.as_u16(),
        });
    }
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Attempt to route `body` to a cloud provider. See module docs for the
/// tri-state return contract.
pub async fn try_route_to_cloud(
    pool: &PgPool,
    model_id: &str,
    body: &Value,
    session_id: Option<&str>,
) -> Option<Result<Response<Body>, Response<Body>>> {
    let provider = match cloud_llm_registry::find_for_model(pool, model_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "cloud_llm: registry lookup failed; skipping cloud path");
            return None;
        }
    };

    // Budget guard (Pillar 3 / PR-T3). Reads
    // `fleet_secrets[budget.daily_usd_cap]` (string parsed as f64). If
    // set and today's `cloud_llm_usage` cost ≥ cap, returns 402 with a
    // clear error so the caller can fall back to a local LLM. Skipped
    // for `oauth_subscription` and `local_bridge` rows since those don't
    // accumulate `cost_usd` (subscription = $0/call). Also skipped when
    // the secret is unset, so default behavior is unchanged.
    if provider.auth_kind == "api_key" {
        if let Some(cap) = pg_get_secret(pool, "budget.daily_usd_cap")
            .await
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            let today_cost: Option<f64> = sqlx::query_scalar(
                "SELECT COALESCE(SUM(cost_usd)::FLOAT8, 0.0)
                   FROM cloud_llm_usage
                  WHERE used_at > NOW() - INTERVAL '24 hours'",
            )
            .fetch_one(pool)
            .await
            .ok();
            let today_cost = today_cost.unwrap_or(0.0);
            if today_cost >= cap {
                tracing::warn!(provider = %provider.id, today_cost, cap,
                    "cloud_llm: budget.daily_usd_cap reached; refusing api_key call");
                return Some(Err(error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    format!(
                        "daily budget cap ${:.2} reached (today: ${:.2}); falling back to local LLM. \
                         Adjust with `ff secrets set budget.daily_usd_cap <usd>` or wait for the 24h window.",
                        cap, today_cost
                    ),
                    "budget_cap_reached",
                )));
            }
        }
    }

    // Resolve the bearer token (or pass-through for local_bridge) based
    // on auth_kind. Three supported variants today (V53):
    //
    //   `api_key`             — pay-per-token vendor billing. secret_key
    //                           points at the API key in fleet_secrets.
    //   `oauth_subscription`  — bearer is the harvested CLI subscription
    //                           token (cred-file → fleet_secrets via
    //                           `ff oauth import`). Same Bearer header
    //                           dance as api_key downstream.
    //   `local_bridge`        — no auth; the call goes to a local
    //                           127.0.0.1:5110X bridge that owns
    //                           credentials internally (PR-D will wire
    //                           the bridge daemon).
    //
    // Legacy `oauth2` (refresh-token flow scaffolded in V26 but never
    // wired) still bails — the new oauth_subscription path replaces it
    // for the subscription use case.
    let api_key = match provider.auth_kind.as_str() {
        "api_key" | "oauth_subscription" => match pg_get_secret(pool, &provider.secret_key).await {
            Ok(Some(k)) if !k.is_empty() => k,
            Ok(_) => {
                let kind_label = if provider.auth_kind == "oauth_subscription" {
                    "OAuth subscription token"
                } else {
                    "API key"
                };
                let hint = if provider.auth_kind == "oauth_subscription" {
                    format!(
                        "Run `ff oauth import {}` on the leader (after `<cli> login`).",
                        provider.id.split('_').next().unwrap_or("claude")
                    )
                } else {
                    format!("Run `ff cloud-llm set-key {}`.", provider.id)
                };
                tracing::warn!(provider = %provider.id, secret_key = %provider.secret_key,
                        "cloud_llm: {} secret is missing/empty", kind_label);
                return Some(Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    format!(
                        "{kind_label} not configured for cloud provider '{}'. {hint}",
                        provider.id
                    ),
                    "cloud_auth_missing",
                )));
            }
            Err(e) => {
                tracing::warn!(provider = %provider.id, error = %e, "cloud_llm: fetch secret failed");
                return Some(Err(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to load credentials for '{}'", provider.id),
                    "cloud_auth_error",
                )));
            }
        },
        "local_bridge" => {
            // Local bridge daemons (51100-51104) don't need an
            // Authorization header — they own credentials internally.
            // Pass an empty string; the call_* helpers send no Bearer
            // when the value is empty (see send_request).
            String::new()
        }
        "oauth2" => {
            // Legacy refresh-token flow — never wired. The new
            // oauth_subscription path replaces it for subscription
            // billing.
            tracing::warn!(provider = %provider.id,
                "cloud_llm: legacy oauth2 auth_kind not wired; falling back to local routing");
            return None;
        }
        other => {
            tracing::warn!(provider = %provider.id, auth_kind = %other,
                "cloud_llm: unknown auth_kind; falling back to local routing");
            return None;
        }
    };

    tracing::info!(provider = %provider.id, model = %model_id, format = %provider.request_format,
        "cloud_llm: routing to cloud provider (api_key=<redacted>)");

    let client = build_client();
    let start = Instant::now();
    let streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let res = match provider.request_format.as_str() {
        "openai_chat" => call_openai_chat(&client, &provider, &api_key, body, streaming).await,
        "anthropic_messages" => call_anthropic_messages(&client, &provider, &api_key, body).await,
        "google_generate_content" => {
            call_google_generate_content(&client, &provider, &api_key, model_id, body).await
        }
        other => {
            return Some(Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "unknown request_format '{other}' for provider '{}'",
                    provider.id
                ),
                "cloud_config_error",
            )));
        }
    };

    let duration_ms = start.elapsed().as_millis().min(i32::MAX as u128) as i32;

    match res {
        Ok(CallOutcome::Json {
            status,
            body: rb,
            tokens_in,
            tokens_out,
        }) => {
            let _ = record_usage(
                pool,
                &provider.id,
                model_id,
                tokens_in,
                tokens_out,
                session_id,
                duration_ms,
            )
            .await;
            Some(Ok(ok_response(rb, &provider, status)))
        }
        Ok(CallOutcome::Stream(resp)) => Some(Ok(resp)),
        Err(CloudCallError::Http { status, message }) => {
            Some(Err(error_response(status, message, "cloud_upstream_error")))
        }
        Err(CloudCallError::Local(msg)) => Some(Err(error_response(
            StatusCode::BAD_GATEWAY,
            msg,
            "cloud_upstream_error",
        ))),
    }
}

enum CallOutcome {
    Json {
        status: StatusCode,
        body: Value,
        tokens_in: Option<i32>,
        tokens_out: Option<i32>,
    },
    Stream(Response<Body>),
}

#[derive(Debug)]
enum CloudCallError {
    Http { status: StatusCode, message: String },
    Local(String),
}

/// Extract an error message from a cloud provider's JSON error body.
fn upstream_error_message(v: &Value, fallback: &str) -> String {
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or(fallback)
        .to_string()
}

// ─── openai_chat ─────────────────────────────────────────────────────────────

async fn call_openai_chat(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    body: &Value,
    streaming: bool,
) -> Result<CallOutcome, CloudCallError> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let req = client
        .post(&url)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .json(body);
    let resp = send_with_429_retry(req)
        .await
        .map_err(|e| CloudCallError::Local(format!("upstream request failed: {e}")))?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if streaming && status.is_success() {
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/event-stream")
            .to_string();
        use futures::TryStreamExt;
        let mapped = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(e.to_string()));
        let axum_resp = Response::builder()
            .status(status)
            .header("content-type", content_type)
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .header("x-accel-buffering", "no")
            .body(Body::from_stream(mapped))
            .map_err(|e| CloudCallError::Local(format!("build streaming response: {e}")))?;
        return Ok(CallOutcome::Stream(axum_resp));
    }

    let value: Value = resp
        .json()
        .await
        .map_err(|e| CloudCallError::Local(format!("parse upstream json: {e}")))?;

    if !status.is_success() {
        return Err(CloudCallError::Http {
            status,
            message: upstream_error_message(&value, "cloud provider returned an error"),
        });
    }

    let (tokens_in, tokens_out) = extract_openai_usage(&value);
    Ok(CallOutcome::Json {
        status,
        body: value,
        tokens_in,
        tokens_out,
    })
}

fn extract_openai_usage(v: &Value) -> (Option<i32>, Option<i32>) {
    let usage = v.get("usage");
    let ti = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);
    let to = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);
    (ti, to)
}

// ─── anthropic_messages ──────────────────────────────────────────────────────

async fn call_anthropic_messages(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    body: &Value,
) -> Result<CallOutcome, CloudCallError> {
    let (anth_body, _) = translate_openai_to_anthropic(body)?;
    let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));

    // Anthropic accepts either `x-api-key` (api-key billing) or
    // `Authorization: Bearer <oauth_token>` (subscription). Pick by
    // auth_kind so the harvested CLI token works for `anthropic_oauth`.
    let mut req = client
        .post(&url)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&anth_body);
    req = if provider.auth_kind == "oauth_subscription" {
        req.bearer_auth(api_key)
    } else {
        req.header("x-api-key", api_key)
    };

    let resp = send_with_429_retry(req)
        .await
        .map_err(|e| CloudCallError::Local(format!("upstream request failed: {e}")))?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let value: Value = resp
        .json()
        .await
        .map_err(|e| CloudCallError::Local(format!("parse upstream json: {e}")))?;

    if !status.is_success() {
        return Err(CloudCallError::Http {
            status,
            message: upstream_error_message(&value, "anthropic returned an error"),
        });
    }

    let openai_shape = anthropic_to_openai_response(&value);
    let tokens_in = value
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);
    let tokens_out = value
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);

    Ok(CallOutcome::Json {
        status,
        body: openai_shape,
        tokens_in,
        tokens_out,
    })
}

/// OpenAI chat body → Anthropic `/v1/messages`.
/// - `system` messages concatenated into top-level `system`
/// - Remaining `user`/`assistant` messages become `messages`
/// - `max_tokens` is REQUIRED by Anthropic; default 4096 when absent.
fn translate_openai_to_anthropic(body: &Value) -> Result<(Value, bool), CloudCallError> {
    let empty: Vec<Value> = Vec::new();
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    let mut system_parts: Vec<String> = Vec::new();
    let mut out_messages: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        match role {
            "system" => {
                if let Some(s) = content.as_str() {
                    system_parts.push(s.to_string());
                }
            }
            "user" | "assistant" => {
                out_messages.push(json!({ "role": role, "content": content }));
            }
            other => {
                tracing::debug!(role = %other, "anthropic translate: dropping unsupported role")
            }
        }
    }

    let mut anth = json!({
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "messages": out_messages,
        "max_tokens": body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096),
    });
    if !system_parts.is_empty() {
        anth["system"] = Value::String(system_parts.join("\n\n"));
    }
    for (src, dst) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop", "stop_sequences"),
    ] {
        if let Some(v) = body.get(src) {
            anth[dst] = v.clone();
        }
    }

    Ok((anth, !system_parts.is_empty()))
}

/// Anthropic `/v1/messages` response → OpenAI chat completion shape.
fn anthropic_to_openai_response(v: &Value) -> Value {
    let id = v
        .get("id")
        .cloned()
        .unwrap_or_else(|| json!("anthropic-msg"));
    let model = v.get("model").cloned().unwrap_or(Value::Null);
    let text = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish_reason = v
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(|s| match s {
            "end_turn" | "stop_sequence" => "stop",
            "max_tokens" => "length",
            other => other,
        })
        .unwrap_or("stop");
    let ti = v
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    let to = v
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|n| n.as_i64())
        .unwrap_or(0);

    json!({
        "id": id, "object": "chat.completion", "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": finish_reason,
        }],
        "usage": { "prompt_tokens": ti, "completion_tokens": to, "total_tokens": ti + to },
    })
}

// ─── google_generate_content ─────────────────────────────────────────────────

async fn call_google_generate_content(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    model_id: &str,
    body: &Value,
) -> Result<CallOutcome, CloudCallError> {
    // Strip the prefix so the bare model name flows to the URL. Both
    // `gemini/` (api_key path) and `gemini-` (oauth path) prefixes are
    // valid model-name leads today.
    let bare_model = model_id
        .strip_prefix("gemini/")
        .or_else(|| model_id.strip_prefix("gemini-"))
        .unwrap_or(model_id);
    // Google supports either `?key=API_KEY` (api-key billing) or
    // `Authorization: Bearer <oauth_token>` (subscription). Pick by
    // auth_kind so the OAuth-harvested token works for `google_oauth`.
    let url = if provider.auth_kind == "oauth_subscription" {
        format!(
            "{}/models/{}:generateContent",
            provider.base_url.trim_end_matches('/'),
            bare_model,
        )
    } else {
        format!(
            "{}/models/{}:generateContent?key={}",
            provider.base_url.trim_end_matches('/'),
            bare_model,
            api_key
        )
    };
    let gbody = translate_openai_to_google(body);

    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&gbody);
    if provider.auth_kind == "oauth_subscription" {
        req = req.bearer_auth(api_key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| CloudCallError::Local(format!("upstream request failed: {e}")))?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let value: Value = resp
        .json()
        .await
        .map_err(|e| CloudCallError::Local(format!("parse upstream json: {e}")))?;

    if !status.is_success() {
        return Err(CloudCallError::Http {
            status,
            message: upstream_error_message(&value, "google returned an error"),
        });
    }

    let openai_shape = google_to_openai_response(&value, bare_model);
    let tokens_in = value
        .get("usageMetadata")
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);
    let tokens_out = value
        .get("usageMetadata")
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32);

    Ok(CallOutcome::Json {
        status,
        body: openai_shape,
        tokens_in,
        tokens_out,
    })
}

/// OpenAI chat body → Google Gemini `:generateContent`.
/// Gemini uses `contents:[{role, parts:[{text}]}]` and `systemInstruction`;
/// role values are `user` / `model` (NOT `assistant`).
fn translate_openai_to_google(body: &Value) -> Value {
    let empty: Vec<Value> = Vec::new();
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = m
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        match role {
            "system" => system_parts.push(text),
            "user" => contents.push(json!({ "role": "user", "parts": [{"text": text}] })),
            "assistant" => contents.push(json!({ "role": "model", "parts": [{"text": text}] })),
            _ => {}
        }
    }

    let mut out = json!({ "contents": contents });
    if !system_parts.is_empty() {
        out["systemInstruction"] = json!({ "parts": [{"text": system_parts.join("\n\n")}] });
    }
    let mut gc = serde_json::Map::new();
    for (src, dst) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_tokens", "maxOutputTokens"),
    ] {
        if let Some(v) = body.get(src) {
            gc.insert(dst.into(), v.clone());
        }
    }
    if !gc.is_empty() {
        out["generationConfig"] = Value::Object(gc);
    }
    out
}

fn google_to_openai_response(v: &Value, model: &str) -> Value {
    let candidates = v.get("candidates").and_then(|c| c.as_array());
    let text = candidates
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish_reason = candidates
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finishReason"))
        .and_then(|s| s.as_str())
        .map(|s| match s {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            other => other,
        })
        .unwrap_or("stop");
    let ti = v
        .get("usageMetadata")
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    let to = v
        .get("usageMetadata")
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|n| n.as_i64())
        .unwrap_or(0);

    json!({
        "id": format!("gemini-{}", chrono::Utc::now().timestamp_millis()),
        "object": "chat.completion", "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": finish_reason,
        }],
        "usage": { "prompt_tokens": ti, "completion_tokens": to, "total_tokens": ti + to },
    })
}

// ─── Usage ledger ────────────────────────────────────────────────────────────

async fn record_usage(
    pool: &PgPool,
    provider_id: &str,
    model: &str,
    tokens_in: Option<i32>,
    tokens_out: Option<i32>,
    session_id: Option<&str>,
    duration_ms: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO cloud_llm_usage
           (provider_id, model, tokens_input, tokens_output, session_id, request_duration_ms)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(provider_id)
    .bind(model)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(session_id)
    .bind(duration_ms)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_to_anthropic_extracts_system_and_defaults_max_tokens() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "You are helpful." },
                { "role": "user", "content": "Hi" },
            ],
        });
        let (anth, had_sys) = translate_openai_to_anthropic(&body).unwrap();
        assert!(had_sys);
        assert_eq!(anth["system"], "You are helpful.");
        assert_eq!(anth["max_tokens"].as_u64().unwrap(), 4096);
        assert_eq!(anth["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn anthropic_to_openai_flattens_content_parts() {
        let resp = json!({
            "id": "msg_1", "model": "claude-x",
            "content": [{ "text": "Hello " }, { "text": "world" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 2 },
        });
        let oai = anthropic_to_openai_response(&resp);
        assert_eq!(oai["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(oai["choices"][0]["finish_reason"], "stop");
        assert_eq!(oai["usage"]["total_tokens"].as_i64().unwrap(), 7);
    }

    #[test]
    fn openai_to_google_maps_roles_and_system() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "Be concise." },
                { "role": "user", "content": "Hi" },
                { "role": "assistant", "content": "Hello" },
            ],
            "max_tokens": 128,
        });
        let g = translate_openai_to_google(&body);
        assert_eq!(g["systemInstruction"]["parts"][0]["text"], "Be concise.");
        assert_eq!(g["contents"][0]["role"], "user");
        assert_eq!(g["contents"][1]["role"], "model");
        assert_eq!(
            g["generationConfig"]["maxOutputTokens"].as_i64().unwrap(),
            128
        );
    }

    #[test]
    fn google_to_openai_shape() {
        let resp = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "ok" }] },
                "finishReason": "STOP",
            }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 1 },
        });
        let oai = google_to_openai_response(&resp, "gemini-1.5-pro");
        assert_eq!(oai["choices"][0]["message"]["content"], "ok");
        assert_eq!(oai["usage"]["total_tokens"].as_i64().unwrap(), 4);
    }
}
