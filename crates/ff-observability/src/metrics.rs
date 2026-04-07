//! Metrics collection — process, node, model, and fleet-wide metrics.
//!
//! Provides structured metric snapshots for monitoring fleet health.
//! Metrics are collected in-memory and can be queried by the dashboard
//! or exported to external systems.

use std::sync::{Arc, Once};

use axum::{extract::Request, middleware::Next, response::Response};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use lazy_static::lazy_static;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry, TextEncoder,
};
use serde::{Deserialize, Serialize};

// ─── Process-Level Metrics ───────────────────────────────────────────────────

/// Resource metrics for a single OS process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMetrics {
    /// Process ID.
    pub pid: u32,
    /// CPU usage percentage (0.0–100.0+).
    pub cpu_percent: f64,
    /// Resident memory in MiB.
    pub memory_mib: f64,
    /// Number of open file descriptors.
    pub open_fds: u64,
    /// Number of active threads.
    pub threads: u64,
    /// Process uptime in seconds.
    pub uptime_secs: u64,
    /// When this snapshot was taken.
    pub sampled_at: DateTime<Utc>,
}

impl Default for ProcessMetrics {
    fn default() -> Self {
        Self {
            pid: 0,
            cpu_percent: 0.0,
            memory_mib: 0.0,
            open_fds: 0,
            threads: 0,
            uptime_secs: 0,
            sampled_at: Utc::now(),
        }
    }
}

// ─── Node-Level Metrics ──────────────────────────────────────────────────────

/// Aggregate metrics for a fleet node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetrics {
    /// Node name (e.g. "taylor", "james").
    pub node_name: String,
    /// Overall CPU usage percentage.
    pub cpu_percent: f64,
    /// Memory used in GiB.
    pub memory_used_gib: f64,
    /// Total memory in GiB.
    pub memory_total_gib: f64,
    /// GPU utilization percentage (0–100), if applicable.
    pub gpu_percent: Option<f64>,
    /// GPU memory used in GiB, if applicable.
    pub gpu_memory_used_gib: Option<f64>,
    /// Disk usage percentage of primary volume.
    pub disk_percent: f64,
    /// Network bytes received since last sample.
    pub net_rx_bytes: u64,
    /// Network bytes sent since last sample.
    pub net_tx_bytes: u64,
    /// Number of active inference processes on this node.
    pub active_inference_count: u32,
    /// Load average (1 min).
    pub load_avg_1m: f64,
    /// When this snapshot was taken.
    pub sampled_at: DateTime<Utc>,
}

impl NodeMetrics {
    /// Returns `true` if this node is under heavy load.
    pub fn is_high_load(&self, cpu_threshold: f64, mem_ratio_threshold: f64) -> bool {
        let mem_ratio = if self.memory_total_gib > 0.0 {
            self.memory_used_gib / self.memory_total_gib
        } else {
            0.0
        };
        self.cpu_percent >= cpu_threshold || mem_ratio >= mem_ratio_threshold
    }

    /// Memory utilization as a ratio (0.0–1.0).
    pub fn memory_utilization(&self) -> f64 {
        if self.memory_total_gib > 0.0 {
            self.memory_used_gib / self.memory_total_gib
        } else {
            0.0
        }
    }
}

// ─── Model-Level Metrics ─────────────────────────────────────────────────────

/// Metrics for a single model endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetrics {
    /// Model identifier (e.g. "qwen3-32b-q4").
    pub model_id: String,
    /// Node hosting this model.
    pub node_name: String,
    /// Total inference requests served.
    pub total_requests: u64,
    /// Requests currently in flight.
    pub active_requests: u32,
    /// Average latency in milliseconds.
    pub avg_latency_ms: f64,
    /// p95 latency in milliseconds.
    pub p95_latency_ms: f64,
    /// p99 latency in milliseconds.
    pub p99_latency_ms: f64,
    /// Total tokens generated.
    pub total_tokens_generated: u64,
    /// Average tokens per second.
    pub avg_tokens_per_sec: f64,
    /// Number of failed requests.
    pub error_count: u64,
    /// When this snapshot was taken.
    pub sampled_at: DateTime<Utc>,
}

impl ModelMetrics {
    /// Error rate as a ratio (0.0–1.0).
    pub fn error_rate(&self) -> f64 {
        if self.total_requests > 0 {
            self.error_count as f64 / self.total_requests as f64
        } else {
            0.0
        }
    }
}

// ─── Fleet-Level Metrics ─────────────────────────────────────────────────────

/// Aggregate metrics across the entire fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMetrics {
    /// Number of nodes online.
    pub nodes_online: u32,
    /// Number of nodes offline.
    pub nodes_offline: u32,
    /// Number of models currently loaded.
    pub models_loaded: u32,
    /// Total inference requests fleet-wide (since boot).
    pub total_requests: u64,
    /// Requests in the last minute.
    pub requests_last_minute: u64,
    /// Average fleet-wide CPU usage.
    pub avg_cpu_percent: f64,
    /// Average fleet-wide memory utilization ratio.
    pub avg_memory_utilization: f64,
    /// Total tokens generated fleet-wide.
    pub total_tokens_generated: u64,
    /// When this snapshot was taken.
    pub sampled_at: DateTime<Utc>,
}

impl Default for FleetMetrics {
    fn default() -> Self {
        Self {
            nodes_online: 0,
            nodes_offline: 0,
            models_loaded: 0,
            total_requests: 0,
            requests_last_minute: 0,
            avg_cpu_percent: 0.0,
            avg_memory_utilization: 0.0,
            total_tokens_generated: 0,
            sampled_at: Utc::now(),
        }
    }
}

// ─── Metrics Collector ───────────────────────────────────────────────────────

/// Thread-safe in-memory metrics collector.
///
/// Stores the latest metric snapshots keyed by node or model.
/// Other crates can push metric updates, and the dashboard reads them.
#[derive(Debug, Clone)]
pub struct MetricsCollector {
    /// Per-node metrics (key = node name).
    pub node_metrics: Arc<DashMap<String, NodeMetrics>>,
    /// Per-model metrics (key = "model_id@node_name").
    pub model_metrics: Arc<DashMap<String, ModelMetrics>>,
    /// Per-process metrics (key = "node_name:pid").
    pub process_metrics: Arc<DashMap<String, ProcessMetrics>>,
}

impl MetricsCollector {
    /// Create a new empty metrics collector.
    pub fn new() -> Self {
        Self {
            node_metrics: Arc::new(DashMap::new()),
            model_metrics: Arc::new(DashMap::new()),
            process_metrics: Arc::new(DashMap::new()),
        }
    }

    /// Record a node metrics snapshot.
    pub fn record_node(&self, metrics: NodeMetrics) {
        self.node_metrics.insert(metrics.node_name.clone(), metrics);
    }

    /// Record a model metrics snapshot.
    pub fn record_model(&self, metrics: ModelMetrics) {
        let key = format!("{}@{}", metrics.model_id, metrics.node_name);
        self.model_metrics.insert(key, metrics);
    }

    /// Record a process metrics snapshot.
    pub fn record_process(&self, node_name: &str, metrics: ProcessMetrics) {
        let key = format!("{}:{}", node_name, metrics.pid);
        self.process_metrics.insert(key, metrics);
    }

    /// Compute fleet-wide aggregate metrics from current node snapshots.
    pub fn fleet_aggregate(&self) -> FleetMetrics {
        let mut online = 0u32;
        let mut total_cpu = 0.0f64;
        let mut total_mem_util = 0.0f64;
        let mut count = 0u32;

        for entry in self.node_metrics.iter() {
            online += 1;
            total_cpu += entry.value().cpu_percent;
            total_mem_util += entry.value().memory_utilization();
            count += 1;
        }

        let mut total_requests = 0u64;
        let mut total_tokens = 0u64;
        let mut models_loaded = 0u32;

        for entry in self.model_metrics.iter() {
            models_loaded += 1;
            total_requests += entry.value().total_requests;
            total_tokens += entry.value().total_tokens_generated;
        }

        FleetMetrics {
            nodes_online: online,
            nodes_offline: 0, // Caller supplies offline count from discovery.
            models_loaded,
            total_requests,
            requests_last_minute: 0, // Requires windowed counter — future work.
            avg_cpu_percent: if count > 0 {
                total_cpu / count as f64
            } else {
                0.0
            },
            avg_memory_utilization: if count > 0 {
                total_mem_util / count as f64
            } else {
                0.0
            },
            total_tokens_generated: total_tokens,
            sampled_at: Utc::now(),
        }
    }

    /// Get a snapshot of all node metrics.
    pub fn all_node_metrics(&self) -> Vec<NodeMetrics> {
        self.node_metrics
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get a snapshot of all model metrics.
    pub fn all_model_metrics(&self) -> Vec<ModelMetrics> {
        self.model_metrics
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Remove stale entries older than `max_age` seconds.
    pub fn evict_stale(&self, max_age_secs: i64) {
        let cutoff = Utc::now() - chrono::Duration::seconds(max_age_secs);

        self.node_metrics.retain(|_, v| v.sampled_at > cutoff);
        self.model_metrics.retain(|_, v| v.sampled_at > cutoff);
        self.process_metrics.retain(|_, v| v.sampled_at > cutoff);
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Prometheus Metrics Export ────────────────────────────────────────────────

lazy_static! {
    /// Global Prometheus registry for ForgeFleet metrics.
    pub static ref PROM_REGISTRY: Registry =
        Registry::new_custom(Some("forgefleet".into()), None)
            .expect("failed to create prometheus registry");

    // ── HTTP metrics ─────────────────────────────────────────────────

    /// Total HTTP requests (labels: method, path, status).
    pub static ref HTTP_REQUESTS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new("http_requests_total", "Total HTTP requests"),
        &["method", "path", "status"],
    ).unwrap();

    /// HTTP request duration in seconds (labels: method, path).
    pub static ref HTTP_REQUEST_DURATION_SECONDS: HistogramVec = HistogramVec::new(
        HistogramOpts::new(
            "http_request_duration_seconds",
            "HTTP request duration in seconds",
        )
        .buckets(vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
        &["method", "path"],
    ).unwrap();

    // ── LLM proxy metrics ────────────────────────────────────────────

    /// Total LLM proxy requests (labels: model, tier, backend, status).
    pub static ref LLM_PROXY_REQUESTS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new("llm_proxy_requests_total", "Total LLM proxy requests"),
        &["model", "tier", "backend", "status"],
    ).unwrap();

    /// LLM proxy request duration in seconds (labels: model, tier).
    pub static ref LLM_PROXY_DURATION_SECONDS: HistogramVec = HistogramVec::new(
        HistogramOpts::new(
            "llm_proxy_duration_seconds",
            "LLM proxy request duration in seconds",
        )
        .buckets(vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]),
        &["model", "tier"],
    ).unwrap();

    // ── Node & model health ──────────────────────────────────────────

    /// Node health gauge (labels: node_name, status). 1 = healthy, 0 = unhealthy.
    pub static ref NODE_HEALTH: GaugeVec = GaugeVec::new(
        Opts::new("node_health", "Node health (1=healthy, 0=unhealthy)"),
        &["node_name", "status"],
    ).unwrap();

    /// Model health gauge (labels: node_name, model_name). 1 = loaded, 0 = unloaded.
    pub static ref MODEL_HEALTH: GaugeVec = GaugeVec::new(
        Opts::new("model_health", "Model health (1=loaded, 0=unloaded)"),
        &["node_name", "model_name"],
    ).unwrap();

    // ── Infrastructure gauges ────────────────────────────────────────

    /// Number of active connections.
    pub static ref ACTIVE_CONNECTIONS: IntGauge = IntGauge::new(
        "active_connections",
        "Number of active connections",
    ).unwrap();

    /// Database connection pool size.
    pub static ref DB_POOL_SIZE: IntGauge = IntGauge::new(
        "db_pool_size",
        "Database connection pool size",
    ).unwrap();

    /// Replication lag in seconds (labels: follower_name).
    pub static ref REPLICATION_LAG_SECONDS: GaugeVec = GaugeVec::new(
        Opts::new("replication_lag_seconds", "Replication lag in seconds"),
        &["follower_name"],
    ).unwrap();

    /// Number of pending tasks in queue.
    pub static ref TASK_QUEUE_DEPTH: IntGauge = IntGauge::new(
        "task_queue_depth",
        "Number of pending tasks in queue",
    ).unwrap();

    // ── Operational counters ─────────────────────────────────────────

    /// Total leader election events.
    pub static ref LEADER_ELECTIONS_TOTAL: IntCounter = IntCounter::new(
        "leader_elections_total",
        "Total leader election events",
    ).unwrap();

    /// Self-update attempts (labels: status).
    pub static ref SELF_UPDATES_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new("self_updates_total", "Self-update attempts"),
        &["status"],
    ).unwrap();
}

static PROM_INIT: Once = Once::new();

/// Initialize and register all Prometheus metrics with the global registry.
///
/// Safe to call multiple times — only the first invocation registers.
pub fn init_prometheus_metrics() {
    PROM_INIT.call_once(|| {
        let r = &*PROM_REGISTRY;
        r.register(Box::new(HTTP_REQUESTS_TOTAL.clone())).unwrap();
        r.register(Box::new(HTTP_REQUEST_DURATION_SECONDS.clone()))
            .unwrap();
        r.register(Box::new(LLM_PROXY_REQUESTS_TOTAL.clone()))
            .unwrap();
        r.register(Box::new(LLM_PROXY_DURATION_SECONDS.clone()))
            .unwrap();
        r.register(Box::new(NODE_HEALTH.clone())).unwrap();
        r.register(Box::new(MODEL_HEALTH.clone())).unwrap();
        r.register(Box::new(ACTIVE_CONNECTIONS.clone())).unwrap();
        r.register(Box::new(DB_POOL_SIZE.clone())).unwrap();
        r.register(Box::new(REPLICATION_LAG_SECONDS.clone()))
            .unwrap();
        r.register(Box::new(TASK_QUEUE_DEPTH.clone())).unwrap();
        r.register(Box::new(LEADER_ELECTIONS_TOTAL.clone()))
            .unwrap();
        r.register(Box::new(SELF_UPDATES_TOTAL.clone())).unwrap();
    });
}

/// Encode all registered metrics in Prometheus text exposition format.
pub fn metrics_handler() -> String {
    let encoder = TextEncoder::new();
    let families = PROM_REGISTRY.gather();
    let mut buf = Vec::new();
    encoder.encode(&families, &mut buf).unwrap_or_default();
    String::from_utf8(buf).unwrap_or_default()
}

/// Normalize a request path for metric labels to avoid high cardinality.
///
/// Replaces UUID-like segments and numeric IDs with `:id`.
pub fn normalize_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg.len() == 36 && seg.chars().filter(|c| *c == '-').count() == 4 {
                ":id"
            } else if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()) {
                ":id"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

// ─── Axum Middleware ─────────────────────────────────────────────────────────

/// Axum middleware that records HTTP request count and duration for every request.
///
/// Skips the `/metrics` endpoint itself to avoid self-referential counting.
pub async fn prometheus_metrics_middleware(req: Request, next: Next) -> Response {
    // Don't instrument the metrics scrape endpoint.
    if req.uri().path() == "/metrics" {
        return next.run(req).await;
    }

    let method = req.method().to_string();
    let path = normalize_path(req.uri().path());
    let start = std::time::Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16().to_string();
    let duration = start.elapsed().as_secs_f64();

    HTTP_REQUESTS_TOTAL
        .with_label_values(&[&method, &path, &status])
        .inc();
    HTTP_REQUEST_DURATION_SECONDS
        .with_label_values(&[&method, &path])
        .observe(duration);

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node(name: &str, cpu: f64, mem_used: f64, mem_total: f64) -> NodeMetrics {
        NodeMetrics {
            node_name: name.to_string(),
            cpu_percent: cpu,
            memory_used_gib: mem_used,
            memory_total_gib: mem_total,
            gpu_percent: None,
            gpu_memory_used_gib: None,
            disk_percent: 40.0,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            active_inference_count: 1,
            load_avg_1m: cpu / 10.0,
            sampled_at: Utc::now(),
        }
    }

    #[test]
    fn test_high_load_detection() {
        let m = sample_node("taylor", 95.0, 100.0, 128.0);
        assert!(m.is_high_load(90.0, 0.9));
        assert!(!m.is_high_load(99.0, 0.9));
    }

    #[test]
    fn test_model_error_rate() {
        let m = ModelMetrics {
            model_id: "qwen-9b".into(),
            node_name: "taylor".into(),
            total_requests: 100,
            active_requests: 0,
            avg_latency_ms: 50.0,
            p95_latency_ms: 80.0,
            p99_latency_ms: 120.0,
            total_tokens_generated: 50_000,
            avg_tokens_per_sec: 45.0,
            error_count: 5,
            sampled_at: Utc::now(),
        };
        assert!((m.error_rate() - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fleet_aggregate() {
        let collector = MetricsCollector::new();
        collector.record_node(sample_node("taylor", 50.0, 64.0, 128.0));
        collector.record_node(sample_node("james", 80.0, 20.0, 24.0));

        let agg = collector.fleet_aggregate();
        assert_eq!(agg.nodes_online, 2);
        assert!((agg.avg_cpu_percent - 65.0).abs() < f64::EPSILON);
    }
}
