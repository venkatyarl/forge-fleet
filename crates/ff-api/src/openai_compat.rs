//! OpenAI API compatibility helpers.
//!
//! Provides request/response types matching the OpenAI `/v1/chat/completions`
//! schema, SSE streaming proxying, and standardised error responses.
//!
//! These are re-exports and thin wrappers over the canonical types in
//! [`crate::types`], plus streaming helpers that proxy backend SSE chunks
//! through to the client.

use axum::{
    body::Body,
    http::{Response, StatusCode, header},
};
use bytes::Bytes;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tracing::debug;

// ─── Re-exports from types ──────────────────────────────────────────────────

pub use crate::types::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatDelta, ChatMessage, ErrorBody,
    ErrorEnvelope, Usage,
};

// ─── Additional Streaming Types ──────────────────────────────────────────────

/// A single server-sent event chunk for streaming chat completions.
///
/// Matches the OpenAI `chat.completion.chunk` schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A choice within a streaming chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: usize,
    #[serde(default)]
    pub delta: Option<ChatDelta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

// ─── Response Builders ───────────────────────────────────────────────────────

/// Build an OpenAI-format error response.
pub fn error_response(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &str,
) -> Response<Body> {
    let body = serde_json::to_vec(&ErrorEnvelope {
        error: ErrorBody {
            message: message.into(),
            r#type: error_type.to_string(),
        },
    })
    .unwrap_or_default();

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Proxy a non-streaming upstream response body through to the client,
/// preserving the status code and content-type.
pub async fn passthrough_response(upstream: reqwest::Response) -> Result<Response<Body>, String> {
    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| format!("failed to read upstream body: {e}"))?;

    let mut builder = Response::builder().status(status.as_u16());
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    builder
        .body(Body::from(bytes))
        .map_err(|e| format!("failed to build response: {e}"))
}

/// Proxy a streaming SSE response from the upstream backend to the client.
///
/// The upstream `reqwest::Response` is expected to produce `text/event-stream`
/// chunks. We proxy them through as-is, preserving the SSE format. The
/// response uses chunked transfer encoding with appropriate headers.
pub async fn passthrough_streaming_response(
    upstream: reqwest::Response,
) -> Result<Response<Body>, String> {
    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| header::HeaderValue::from_static("text/event-stream; charset=utf-8"));

    debug!(status = %status, "proxying streaming response");

    let byte_stream = upstream.bytes_stream().map(|chunk| {
        chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))
    });

    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("X-Accel-Buffering", "no") // Disable nginx buffering
        .body(Body::from_stream(byte_stream))
        .map_err(|e| format!("failed to build streaming response: {e}"))
}

/// Build a synthetic SSE error event and terminal `[DONE]` marker.
///
/// Useful when the backend returns an error mid-stream or we need to send a
/// clean error in SSE format.
pub fn sse_error_event(message: &str) -> Bytes {
    let event = serde_json::json!({
        "error": {
            "message": message,
            "type": "upstream_error"
        }
    });
    Bytes::from(format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    ))
}

// ─── Request Validation ──────────────────────────────────────────────────────

/// Validate a ChatCompletionRequest before routing.
///
/// Returns `Ok(())` if valid, or an error message describing the issue.
pub fn validate_request(req: &ChatCompletionRequest) -> Result<(), String> {
    if req.model.is_empty() {
        return Err("model field is required".to_string());
    }

    if req.messages.is_empty() {
        return Err("messages array must not be empty".to_string());
    }

    if let Some(temp) = req.temperature {
        if !(0.0..=2.0).contains(&temp) {
            return Err(format!("temperature must be between 0 and 2, got {temp}"));
        }
    }

    if let Some(top_p) = req.top_p {
        if !(0.0..=1.0).contains(&top_p) {
            return Err(format!("top_p must be between 0 and 1, got {top_p}"));
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello".to_string()),
                name: None,
                extra: HashMap::new(),
            }],
            temperature: Some(0.7),
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_validate_valid_request() {
        assert!(validate_request(&sample_request()).is_ok());
    }

    #[test]
    fn test_validate_empty_model() {
        let mut req = sample_request();
        req.model = String::new();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_validate_empty_messages() {
        let mut req = sample_request();
        req.messages = vec![];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_validate_temperature_out_of_range() {
        let mut req = sample_request();
        req.temperature = Some(3.0);
        assert!(validate_request(&req).is_err());

        req.temperature = Some(-0.5);
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_validate_top_p_out_of_range() {
        let mut req = sample_request();
        req.top_p = Some(1.5);
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_error_response_json() {
        let resp = error_response(StatusCode::BAD_REQUEST, "test error", "bad_request");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_sse_error_event() {
        let event = sse_error_event("something went wrong");
        let text = std::str::from_utf8(&event).unwrap();
        assert!(text.starts_with("data: "));
        assert!(text.contains("something went wrong"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn test_chat_completion_chunk_serialization() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Some(ChatDelta {
                    role: Some("assistant".to_string()),
                    content: Some(Value::String("Hello".to_string())),
                }),
                finish_reason: None,
            }],
            usage: None,
            extra: HashMap::new(),
        };

        let json = serde_json::to_value(&chunk).unwrap();
        assert_eq!(json["object"], "chat.completion.chunk");
        assert_eq!(json["choices"][0]["delta"]["content"], "Hello");
    }
}
