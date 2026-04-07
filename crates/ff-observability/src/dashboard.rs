//! Dashboard aggregation and API routes for fleet observability snapshots.
//!
//! This module provides:
//! - in-memory dashboard state
//! - aggregate snapshot builders
//! - axum routes for status endpoints

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ff_core::{Model, Node, NodeStatus};
use serde::{Deserialize, Serialize};

use crate::alerting::{Alert, AlertEngine, AlertSeverity};
use crate::events::{EventRecord, InMemoryEventSink};
use crate::log_ingest::{LogEntry, LogIngestor};
use crate::metrics::{MetricsCollector, NodeMetrics};

// ─── Snapshot Types ──────────────────────────────────────────────────────────

/// Per-node summary for dashboard display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub status: NodeStatus,
    pub model_count: usize,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub cpu_percent: Option<f64>,
    pub memory_utilization: Option<f64>,
    pub gpu_percent: Option<f64>,
    pub active_alerts: usize,
}

/// Per-model summary for dashboard display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub id: String,
    pub name: String,
    pub tier: ff_core::Tier,
    pub runtime: ff_core::Runtime,
    pub nodes: Vec<String>,
    pub total_requests: u64,
    pub active_requests: u32,
    pub avg_latency_ms: f64,
    pub error_rate: f64,
}

/// Fleet-wide aggregate snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub captured_at: DateTime<Utc>,
    pub fleet_name: String,
    pub nodes_total: usize,
    pub nodes_online: usize,
    pub nodes_offline: usize,
    pub models_total: usize,
    pub total_requests: u64,
    pub avg_cpu_percent: f64,
    pub avg_memory_utilization: f64,
    pub active_alerts_total: usize,
    pub active_alerts_warning: usize,
    pub active_alerts_critical: usize,
    pub recent_event_count: usize,
    pub recent_error_log_count: usize,
}

/// Health endpoint payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub timestamp: DateTime<Utc>,
    pub service: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecentQuery {
    #[serde(default = "default_recent_limit")]
    limit: usize,
}

fn default_recent_limit() -> usize {
    50
}

// ─── Dashboard State ─────────────────────────────────────────────────────────

/// Shared state backing dashboard APIs.
#[derive(Debug, Clone)]
pub struct DashboardState {
    /// Fleet name for display.
    pub fleet_name: String,
    /// Canonical node inventory.
    pub nodes: Arc<DashMap<String, Node>>,
    /// Canonical model inventory.
    pub models: Arc<DashMap<String, Model>>,
    /// Live metrics collector.
    pub metrics: MetricsCollector,
    /// Event sink.
    pub events: InMemoryEventSink,
    /// Log ingestor.
    pub logs: LogIngestor,
    /// Alert engine.
    pub alerts: AlertEngine,
}

impl DashboardState {
    /// Build dashboard state with default observability components.
    pub fn new(fleet_name: impl Into<String>) -> Self {
        Self {
            fleet_name: fleet_name.into(),
            nodes: Arc::new(DashMap::new()),
            models: Arc::new(DashMap::new()),
            metrics: MetricsCollector::new(),
            events: InMemoryEventSink::new(10_000),
            logs: LogIngestor::new(25_000, 5_000),
            alerts: AlertEngine::with_defaults(),
        }
    }

    /// Insert/update a node in inventory.
    pub fn upsert_node(&self, node: Node) {
        self.nodes.insert(node.name.clone(), node);
    }

    /// Insert/update a model in inventory.
    pub fn upsert_model(&self, model: Model) {
        self.models.insert(model.id.clone(), model);
    }

    /// Build per-node dashboard summaries.
    pub fn node_summaries(&self) -> Vec<NodeSummary> {
        self.nodes
            .iter()
            .map(|entry| {
                let node = entry.value();
                let node_metrics: Option<NodeMetrics> = self
                    .metrics
                    .node_metrics
                    .get(&node.name)
                    .map(|m| m.value().clone());

                let active_alerts = self
                    .alerts
                    .active_alerts()
                    .into_iter()
                    .filter(|a| a.node.as_deref() == Some(node.name.as_str()))
                    .count();

                NodeSummary {
                    name: node.name.clone(),
                    host: node.host.clone(),
                    port: node.port,
                    status: node.status,
                    model_count: node.models.len(),
                    last_heartbeat: node.last_heartbeat,
                    cpu_percent: node_metrics.as_ref().map(|m| m.cpu_percent),
                    memory_utilization: node_metrics.as_ref().map(|m| m.memory_utilization()),
                    gpu_percent: node_metrics.and_then(|m| m.gpu_percent),
                    active_alerts,
                }
            })
            .collect()
    }

    /// Build per-model dashboard summaries.
    pub fn model_summaries(&self) -> Vec<ModelSummary> {
        self.models
            .iter()
            .map(|entry| {
                let model = entry.value();

                // Aggregate metrics across nodes for this model ID.
                let matching_metrics: Vec<_> = self
                    .metrics
                    .all_model_metrics()
                    .into_iter()
                    .filter(|m| m.model_id == model.id)
                    .collect();

                let total_requests = matching_metrics.iter().map(|m| m.total_requests).sum();
                let active_requests = matching_metrics.iter().map(|m| m.active_requests).sum();

                let avg_latency_ms = if matching_metrics.is_empty() {
                    0.0
                } else {
                    let sum: f64 = matching_metrics.iter().map(|m| m.avg_latency_ms).sum();
                    sum / matching_metrics.len() as f64
                };

                let error_rate = if matching_metrics.is_empty() {
                    0.0
                } else {
                    let total_errors: u64 = matching_metrics.iter().map(|m| m.error_count).sum();
                    let total_reqs: u64 = matching_metrics.iter().map(|m| m.total_requests).sum();
                    if total_reqs > 0 {
                        total_errors as f64 / total_reqs as f64
                    } else {
                        0.0
                    }
                };

                ModelSummary {
                    id: model.id.clone(),
                    name: model.name.clone(),
                    tier: model.tier,
                    runtime: model.runtime,
                    nodes: model.nodes.clone(),
                    total_requests,
                    active_requests,
                    avg_latency_ms,
                    error_rate,
                }
            })
            .collect()
    }

    /// Build a fleet-wide aggregate snapshot.
    pub async fn fleet_snapshot(&self) -> FleetSnapshot {
        let node_summaries = self.node_summaries();
        let models_total = self.models.len();
        let fleet_metrics = self.metrics.fleet_aggregate();

        let active_alerts = self.alerts.active_alerts();
        let active_alerts_warning = active_alerts
            .iter()
            .filter(|a| a.severity == AlertSeverity::Warning)
            .count();
        let active_alerts_critical = active_alerts
            .iter()
            .filter(|a| a.severity == AlertSeverity::Critical)
            .count();

        let recent_event_count = self.events.recent(100).await.len();
        let recent_error_log_count = self.logs.recent_errors(100).await.len();

        FleetSnapshot {
            captured_at: Utc::now(),
            fleet_name: self.fleet_name.clone(),
            nodes_total: node_summaries.len(),
            nodes_online: node_summaries
                .iter()
                .filter(|n| matches!(n.status, NodeStatus::Online | NodeStatus::Degraded))
                .count(),
            nodes_offline: node_summaries
                .iter()
                .filter(|n| matches!(n.status, NodeStatus::Offline))
                .count(),
            models_total,
            total_requests: fleet_metrics.total_requests,
            avg_cpu_percent: fleet_metrics.avg_cpu_percent,
            avg_memory_utilization: fleet_metrics.avg_memory_utilization,
            active_alerts_total: active_alerts.len(),
            active_alerts_warning,
            active_alerts_critical,
            recent_event_count,
            recent_error_log_count,
        }
    }

    /// Build axum router for dashboard API endpoints.
    pub fn router(state: Arc<Self>) -> Router {
        Router::new()
            .route("/health", get(health_handler))
            .route("/snapshot", get(snapshot_handler))
            .route("/nodes", get(nodes_handler))
            .route("/models", get(models_handler))
            .route("/alerts", get(alerts_handler))
            .route("/events/recent", get(events_recent_handler))
            .route("/logs/errors", get(error_logs_handler))
            .with_state(state)
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        timestamp: Utc::now(),
        service: "ff-observability".to_string(),
        version: crate::VERSION.to_string(),
    })
}

async fn snapshot_handler(State(state): State<Arc<DashboardState>>) -> Json<FleetSnapshot> {
    Json(state.fleet_snapshot().await)
}

async fn nodes_handler(State(state): State<Arc<DashboardState>>) -> Json<Vec<NodeSummary>> {
    Json(state.node_summaries())
}

async fn models_handler(State(state): State<Arc<DashboardState>>) -> Json<Vec<ModelSummary>> {
    Json(state.model_summaries())
}

async fn alerts_handler(State(state): State<Arc<DashboardState>>) -> Json<Vec<Alert>> {
    Json(state.alerts.active_alerts())
}

async fn events_recent_handler(
    State(state): State<Arc<DashboardState>>,
    Query(query): Query<RecentQuery>,
) -> Json<Vec<EventRecord>> {
    Json(state.events.recent(query.limit).await)
}

async fn error_logs_handler(
    State(state): State<Arc<DashboardState>>,
    Query(query): Query<RecentQuery>,
) -> Json<Vec<LogEntry>> {
    Json(state.logs.recent_errors(query.limit).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::{GpuType, Hardware, Interconnect, MemoryType, Role, Runtime};
    use uuid::Uuid;

    #[tokio::test]
    async fn test_snapshot_empty() {
        let state = DashboardState::new("forgefleet-test");
        let snap = state.fleet_snapshot().await;

        assert_eq!(snap.fleet_name, "forgefleet-test");
        assert_eq!(snap.nodes_total, 0);
        assert_eq!(snap.models_total, 0);
    }

    #[tokio::test]
    async fn test_node_summary_and_snapshot() {
        let state = DashboardState::new("forgefleet-test");

        state.upsert_node(Node {
            id: Uuid::new_v4(),
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            role: Role::Leader,
            election_priority: 1,
            status: NodeStatus::Online,
            hardware: Hardware {
                os: ff_core::OsType::MacOs,
                cpu_model: "Apple M4 Max".into(),
                cpu_cores: 16,
                gpu: GpuType::AppleSilicon,
                gpu_model: None,
                memory_gib: 128,
                memory_type: MemoryType::Unified,
                interconnect: Interconnect::Ethernet10g,
                runtimes: vec![Runtime::LlamaCpp],
            },
            models: vec!["qwen3-32b-q4".into()],
            last_heartbeat: Some(Utc::now()),
            registered_at: Utc::now(),
        });

        let nodes = state.node_summaries();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "taylor");

        let snap = state.fleet_snapshot().await;
        assert_eq!(snap.nodes_total, 1);
        assert_eq!(snap.nodes_online, 1);
    }
}
