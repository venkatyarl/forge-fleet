//! `ff-observability` — ForgeFleet observability primitives.
//!
//! This crate provides the full observability stack for ForgeFleet:
//!
//! - **telemetry** — Initialize tracing, JSON structured logging, span propagation
//! - **metrics** — Process, node, model, and fleet-level metric collection
//! - **events** — Structured fleet event model with pluggable sinks
//! - **log_ingest** — Ingest and buffer structured logs from nodes and agents
//! - **alerting** — Rule-based alerts (node down, model unavailable, high load)
//! - **dashboard** — Aggregate status snapshots and axum API routes

pub mod alerting;
pub mod dashboard;
pub mod events;
pub mod file_logger;
pub mod log_ingest;
pub mod metrics;
pub mod telemetry;
pub mod tracing_ext;

// Re-export the most commonly used items at crate root.
pub use alerting::{Alert, AlertEngine, AlertRule, AlertSeverity};
pub use dashboard::{DashboardState, FleetSnapshot, ModelSummary, NodeSummary};
pub use events::{EventRecord, EventSink, FleetEvent, InMemoryEventSink};
pub use file_logger::FileLogConfig;
pub use log_ingest::{LogBuffer, LogEntry, LogIngestor, LogLevel};
pub use metrics::{
    // Prometheus exports
    ACTIVE_CONNECTIONS,
    DB_POOL_SIZE,
    FleetMetrics,
    HTTP_REQUEST_DURATION_SECONDS,
    HTTP_REQUESTS_TOTAL,
    LEADER_ELECTIONS_TOTAL,
    LLM_PROXY_DURATION_SECONDS,
    LLM_PROXY_REQUESTS_TOTAL,
    MODEL_HEALTH,
    MetricsCollector,
    ModelMetrics,
    NODE_HEALTH,
    NodeMetrics,
    PROM_REGISTRY,
    ProcessMetrics,
    REPLICATION_LAG_SECONDS,
    SELF_UPDATES_TOTAL,
    TASK_QUEUE_DEPTH,
    init_prometheus_metrics,
    metrics_handler,
    normalize_path,
    prometheus_metrics_middleware,
};
pub use telemetry::{TelemetryConfig, init_telemetry};
pub use tracing_ext::{
    SpanExt, TraceStore, TraceSummary, extract_or_generate_trace_id, extract_trace_header,
    global_trace_store, inject_trace_header, new_trace_id, trace_discovery, trace_llm_call,
    trace_replication, trace_request,
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
