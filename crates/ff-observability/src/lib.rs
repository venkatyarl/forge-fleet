//! `ff-observability` — ForgeFleet observability primitives.
//!
//! This crate provides the full observability stack for ForgeFleet:
//!
//! - **telemetry** — Initialize tracing, JSON structured logging, span propagation
//! - **metrics** — Process, node, model, and fleet-level metric collection
//! - **events** — Structured fleet event model with pluggable sinks
//! - **log_ingest** — Ingest and buffer structured logs from nodes and agents
//! - **alerting** — Rule-based alerts (node down, model unavailable, high load)
//! - **alerts** — TTL-based deduplication state for alert delivery
//! - **dashboard** — Aggregate status snapshots and axum API routes

pub mod alerting;
pub mod alerts;
pub mod dashboard;
pub mod events;
pub mod file_logger;
pub mod log_ingest;
pub mod metric_writer;
pub mod metrics;
pub mod telemetry;
pub mod tracing_ext;
pub mod work_queue;

// Re-export the most commonly used items at crate root.
pub use alerting::{Alert, AlertEngine, AlertRule, AlertSeverity};
pub use alerts::{AlertDedupState, AlertDeduplicationState};
pub use dashboard::{DashboardState, FleetSnapshot, ModelSummary, NodeSummary};
pub use events::{EventRecord, EventSink, FleetEvent, InMemoryEventSink};
pub use file_logger::FileLogConfig;
pub use log_ingest::{LogBuffer, LogEntry, LogIngestor, LogLevel};
pub use metric_writer::{NormalizedMetricRow, write_metrics};
pub use metrics::{
    // Prometheus exports
    ACTIVE_CONNECTIONS,
    BUILD_TIMEOUT_COUNT,
    DB_POOL_SIZE,
    FleetMetrics,
    HTTP_REQUEST_DURATION_SECONDS,
    HTTP_REQUESTS_TOTAL,
    LEADER_ELECTIONS_TOTAL,
    LLM_BUDGET_PERCENT_USED,
    LLM_BUDGET_REMAINING_USD,
    LLM_COST_USD_TOTAL,
    LLM_PROXY_DURATION_SECONDS,
    LLM_PROXY_REQUESTS_TOTAL,
    LLM_TOKENS_TOTAL,
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
pub use telemetry::{TelemetryConfig, init_telemetry, init_telemetry_with_extra_layer};
pub use tracing_ext::{
    SpanExt, TraceStore, TraceSummary, extract_or_generate_trace_id, extract_trace_header,
    global_trace_store, inject_trace_header, new_trace_id, trace_discovery, trace_llm_call,
    trace_replication, trace_request,
};
pub use work_queue::{
    WORK_QUEUE_PROCESSING_SECONDS, WORK_QUEUE_SIZE, WORK_QUEUE_SIZE_BY_PRIORITY,
    init_work_queue_metrics, observe_processing_time, set_priority_distribution, set_queue_size,
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
