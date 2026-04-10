//! Fleet Pulse data types — node metrics, fleet snapshots, and events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// System metrics for a single fleet node.
///
/// Published to Redis as JSON with a 30-second TTL.
/// If the key expires, the node is considered offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetrics {
    pub node_name: String,
    pub timestamp: DateTime<Utc>,
    pub cpu_percent: f64,
    pub ram_used_gb: f64,
    pub ram_total_gb: f64,
    pub disk_used_gb: f64,
    pub disk_total_gb: f64,
    pub loaded_models: Vec<String>,
    pub active_tasks: u32,
    pub queue_depth: u32,
    pub tokens_per_sec: f64,
    pub temperature_c: Option<f64>,
    pub uptime_secs: u64,
}

/// Snapshot of the entire fleet at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub timestamp: DateTime<Utc>,
    pub nodes: Vec<NodeMetrics>,
    pub online_count: usize,
    pub total_ram_gb: f64,
    pub total_tokens_per_sec: f64,
}

/// A discrete fleet event published via pub/sub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulseEvent {
    pub event_type: PulseEventType,
    pub node_name: String,
    pub timestamp: DateTime<Utc>,
    pub details: serde_json::Value,
}

/// Types of events that flow through the pulse:events channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PulseEventType {
    NodeOnline,
    NodeOffline,
    ModelLoaded,
    ModelUnloaded,
    TaskStarted,
    TaskCompleted,
    HighLoad,
}
