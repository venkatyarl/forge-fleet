//! Distributed tracing extensions for ForgeFleet.
//!
//! Provides:
//! - **Trace ID generation** — UUID-based request trace IDs
//! - **Span factories** — `trace_request`, `trace_llm_call`, `trace_discovery`, `trace_replication`
//! - **SpanExt trait** — common attribute recording on any [`tracing::Span`]
//! - **TraceStore** — in-memory ring buffer for `/api/traces/recent`

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{Span, info_span};
use uuid::Uuid;

// ─── Trace ID Generation ────────────────────────────────────────────────────

/// Generate a new trace ID based on UUID v4.
pub fn new_trace_id() -> String {
    Uuid::new_v4().to_string()
}

/// Extract a trace ID from an optional header value, or generate a new one.
pub fn extract_or_generate_trace_id(header_value: Option<&str>) -> String {
    header_value
        .filter(|v| !v.is_empty())
        .map(String::from)
        .unwrap_or_else(new_trace_id)
}

// ─── Span Context Propagation ────────────────────────────────────────────────

/// Inject a trace ID into HTTP request headers (for outgoing calls).
pub fn inject_trace_header(headers: &mut axum::http::HeaderMap, trace_id: &str) {
    if let Ok(val) = trace_id.parse() {
        headers.insert("x-trace-id", val);
    }
}

/// Extract a trace ID from HTTP request headers (for incoming calls).
pub fn extract_trace_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

// ─── Span Factories ─────────────────────────────────────────────────────────

/// Create a span for an incoming HTTP request.
///
/// Fields: `trace_id`, `http.method`, `http.path`, `http.status_code`, `elapsed_ms`
pub fn trace_request(trace_id: &str, method: &str, path: &str) -> Span {
    info_span!(
        "http_request",
        trace_id = %trace_id,
        http.method = %method,
        http.path = %path,
        http.status_code = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
    )
}

/// Create a span for an LLM proxy call.
///
/// Fields: `trace_id`, `llm.model`, `llm.tier`, `llm.backend`, `llm.latency_ms`, `llm.status`
pub fn trace_llm_call(trace_id: &str, model: &str, tier: u8, backend: &str) -> Span {
    info_span!(
        "llm_call",
        trace_id = %trace_id,
        llm.model = %model,
        llm.tier = tier,
        llm.backend = %backend,
        llm.latency_ms = tracing::field::Empty,
        llm.status = tracing::field::Empty,
    )
}

/// Create a span for a node discovery scan.
///
/// Fields: `trace_id`, `discovery.scan_type`, `discovery.nodes_found`, `discovery.elapsed_ms`
pub fn trace_discovery(trace_id: &str, scan_type: &str) -> Span {
    info_span!(
        "discovery_scan",
        trace_id = %trace_id,
        discovery.scan_type = %scan_type,
        discovery.nodes_found = tracing::field::Empty,
        discovery.elapsed_ms = tracing::field::Empty,
    )
}

/// Create a span for a DB replication operation.
///
/// Fields: `trace_id`, `replication.operation`, `replication.peer`,
/// `replication.records`, `replication.elapsed_ms`
pub fn trace_replication(trace_id: &str, operation: &str, peer: &str) -> Span {
    info_span!(
        "db_replication",
        trace_id = %trace_id,
        replication.operation = %operation,
        replication.peer = %peer,
        replication.records = tracing::field::Empty,
        replication.elapsed_ms = tracing::field::Empty,
    )
}

// ─── SpanExt Trait ──────────────────────────────────────────────────────────

/// Extension trait for recording common attributes on any [`tracing::Span`].
pub trait SpanExt {
    /// Record an HTTP status code on the span.
    fn record_status(&self, status: u16);
    /// Record elapsed time in milliseconds.
    fn record_elapsed_ms(&self, ms: u64);
    /// Record an error message.
    fn record_error(&self, error: &str);
    /// Record the node name.
    fn record_node(&self, node: &str);
    /// Record the service name.
    fn record_service(&self, service: &str);
    /// Record the number of items (nodes found, records replicated, etc.).
    fn record_count(&self, field: &str, count: usize);
}

impl SpanExt for Span {
    fn record_status(&self, status: u16) {
        self.record("http.status_code", status);
    }

    fn record_elapsed_ms(&self, ms: u64) {
        self.record("elapsed_ms", ms);
    }

    fn record_error(&self, error: &str) {
        self.record("error", error);
    }

    fn record_node(&self, node: &str) {
        self.record("node", node);
    }

    fn record_service(&self, service: &str) {
        self.record("service", service);
    }

    fn record_count(&self, field: &str, count: usize) {
        self.record(field, count as u64);
    }
}

// ─── Trace Summary ──────────────────────────────────────────────────────────

/// Summary of a completed trace/span for the recent traces API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    /// The trace ID linking related spans.
    pub trace_id: String,
    /// Human-readable span name (e.g. "http_request", "llm_call").
    pub span_name: String,
    /// Service that produced the span.
    pub service: String,
    /// When the span started.
    pub started_at: DateTime<Utc>,
    /// Duration of the span in milliseconds.
    pub elapsed_ms: u64,
    /// HTTP status code (if applicable).
    pub status: Option<u16>,
    /// Arbitrary key-value attributes.
    pub attributes: serde_json::Value,
}

// ─── In-Memory Trace Store (Ring Buffer) ─────────────────────────────────────

/// Thread-safe ring buffer that stores the last N [`TraceSummary`] entries.
///
/// Used by the `/api/traces/recent` endpoint. The default capacity is 100.
#[derive(Debug, Clone)]
pub struct TraceStore {
    inner: Arc<Mutex<VecDeque<TraceSummary>>>,
    capacity: usize,
}

impl TraceStore {
    /// Create a new trace store with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Record a trace summary. If the buffer is full, the oldest entry is evicted.
    pub fn record(&self, summary: TraceSummary) {
        if let Ok(mut buf) = self.inner.lock() {
            if buf.len() >= self.capacity {
                buf.pop_front();
            }
            buf.push_back(summary);
        }
    }

    /// Return the most recent `limit` trace summaries (newest first).
    pub fn recent(&self, limit: usize) -> Vec<TraceSummary> {
        self.inner
            .lock()
            .map(|buf| buf.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Number of stored trace summaries.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all stored traces.
    pub fn clear(&self) {
        if let Ok(mut buf) = self.inner.lock() {
            buf.clear();
        }
    }
}

impl Default for TraceStore {
    fn default() -> Self {
        Self::new(100)
    }
}

// ─── Global Trace Store ──────────────────────────────────────────────────────

static GLOBAL_TRACE_STORE: OnceLock<TraceStore> = OnceLock::new();

/// Access the global trace store singleton (capacity 100).
///
/// This is the store used by the gateway trace-ID middleware and
/// the `/api/traces/recent` endpoint.
pub fn global_trace_store() -> &'static TraceStore {
    GLOBAL_TRACE_STORE.get_or_init(TraceStore::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_trace_id_is_valid_uuid() {
        let id = new_trace_id();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn test_extract_or_generate_preserves_existing() {
        let existing = "abc-123";
        assert_eq!(extract_or_generate_trace_id(Some(existing)), "abc-123");
    }

    #[test]
    fn test_extract_or_generate_creates_new_when_none() {
        let id = extract_or_generate_trace_id(None);
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn test_extract_or_generate_creates_new_when_empty() {
        let id = extract_or_generate_trace_id(Some(""));
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn test_trace_store_capacity() {
        let store = TraceStore::new(3);
        for i in 0..5 {
            store.record(TraceSummary {
                trace_id: format!("trace-{i}"),
                span_name: "test".into(),
                service: "test".into(),
                started_at: Utc::now(),
                elapsed_ms: i as u64,
                status: None,
                attributes: serde_json::json!({}),
            });
        }
        assert_eq!(store.len(), 3);
        let recent = store.recent(10);
        assert_eq!(recent[0].trace_id, "trace-4"); // newest first
        assert_eq!(recent[2].trace_id, "trace-2");
    }

    #[test]
    fn test_trace_store_recent_limit() {
        let store = TraceStore::new(10);
        for i in 0..10 {
            store.record(TraceSummary {
                trace_id: format!("trace-{i}"),
                span_name: "test".into(),
                service: "test".into(),
                started_at: Utc::now(),
                elapsed_ms: 0,
                status: None,
                attributes: serde_json::json!({}),
            });
        }
        let recent = store.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].trace_id, "trace-9");
    }

    #[test]
    fn test_span_factories_return_valid_spans() {
        // Just verify they don't panic
        let _s1 = trace_request("tid-1", "GET", "/health");
        let _s2 = trace_llm_call("tid-2", "qwen-32b", 2, "192.168.5.101:51800");
        let _s3 = trace_discovery("tid-3", "full_scan");
        let _s4 = trace_replication("tid-4", "push", "192.168.5.102");
    }

    #[test]
    fn test_global_trace_store_is_singleton() {
        let a = global_trace_store() as *const TraceStore;
        let b = global_trace_store() as *const TraceStore;
        assert_eq!(a, b);
    }
}
