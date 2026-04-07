//! Structured fleet event model and pluggable sinks.
//!
//! Events represent significant things that happen in the fleet:
//! node state changes, model loads, task completions, alerts firing, etc.
//! Sinks receive events for storage, forwarding, or further processing.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

// ─── Event Types ─────────────────────────────────────────────────────────────

/// Category of fleet event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetEvent {
    // ── Node Events ──────────────────────────────────────────────────
    /// A node came online.
    NodeOnline { node: String },
    /// A node went offline.
    NodeOffline { node: String },
    /// A node transitioned to degraded state.
    NodeDegraded { node: String, reason: String },
    /// A node's heartbeat was received.
    NodeHeartbeat { node: String },

    // ── Model Events ─────────────────────────────────────────────────
    /// A model was loaded on a node.
    ModelLoaded { model_id: String, node: String },
    /// A model was unloaded from a node.
    ModelUnloaded { model_id: String, node: String },
    /// A model endpoint became unhealthy.
    ModelUnhealthy {
        model_id: String,
        node: String,
        reason: String,
    },

    // ── Inference Events ─────────────────────────────────────────────
    /// An inference request completed.
    InferenceComplete {
        model_id: String,
        node: String,
        latency_ms: u64,
        tokens: u32,
    },
    /// An inference request failed.
    InferenceFailed {
        model_id: String,
        node: String,
        error: String,
    },
    /// A request was escalated to a higher tier.
    TierEscalation {
        from_tier: u8,
        to_tier: u8,
        reason: String,
    },

    // ── Leader Events ────────────────────────────────────────────────
    /// Leader election completed.
    LeaderElected { node: String },
    /// Leader failover occurred.
    LeaderFailover { old: String, new: String },

    // ── Task Events ──────────────────────────────────────────────────
    /// A task was assigned.
    TaskAssigned { task_id: Uuid, node: String },
    /// A task completed.
    TaskCompleted {
        task_id: Uuid,
        success: bool,
        duration_ms: u64,
    },

    // ── Alert Events ─────────────────────────────────────────────────
    /// An alert fired.
    AlertFired {
        alert_id: String,
        severity: String,
        message: String,
    },
    /// An alert was resolved.
    AlertResolved { alert_id: String },

    // ── System Events ────────────────────────────────────────────────
    /// Fleet startup.
    FleetStartup,
    /// Fleet shutdown.
    FleetShutdown,
    /// A custom / free-form event.
    Custom {
        kind: String,
        payload: serde_json::Value,
    },
}

impl FleetEvent {
    /// Returns a human-readable summary of the event.
    pub fn summary(&self) -> String {
        match self {
            Self::NodeOnline { node } => format!("Node {node} came online"),
            Self::NodeOffline { node } => format!("Node {node} went offline"),
            Self::NodeDegraded { node, reason } => format!("Node {node} degraded: {reason}"),
            Self::NodeHeartbeat { node } => format!("Heartbeat from {node}"),
            Self::ModelLoaded { model_id, node } => format!("Model {model_id} loaded on {node}"),
            Self::ModelUnloaded { model_id, node } => {
                format!("Model {model_id} unloaded from {node}")
            }
            Self::ModelUnhealthy {
                model_id,
                node,
                reason,
            } => {
                format!("Model {model_id} on {node} unhealthy: {reason}")
            }
            Self::InferenceComplete {
                model_id,
                latency_ms,
                ..
            } => {
                format!("Inference on {model_id} completed in {latency_ms}ms")
            }
            Self::InferenceFailed {
                model_id, error, ..
            } => {
                format!("Inference on {model_id} failed: {error}")
            }
            Self::TierEscalation {
                from_tier,
                to_tier,
                reason,
            } => {
                format!("Tier escalation {from_tier}→{to_tier}: {reason}")
            }
            Self::LeaderElected { node } => format!("Leader elected: {node}"),
            Self::LeaderFailover { old, new } => format!("Leader failover {old}→{new}"),
            Self::TaskAssigned { task_id, node } => format!("Task {task_id} assigned to {node}"),
            Self::TaskCompleted {
                task_id,
                success,
                duration_ms,
            } => {
                let status = if *success { "succeeded" } else { "failed" };
                format!("Task {task_id} {status} in {duration_ms}ms")
            }
            Self::AlertFired {
                alert_id, message, ..
            } => format!("Alert {alert_id}: {message}"),
            Self::AlertResolved { alert_id } => format!("Alert {alert_id} resolved"),
            Self::FleetStartup => "Fleet started".to_string(),
            Self::FleetShutdown => "Fleet shutting down".to_string(),
            Self::Custom { kind, .. } => format!("Custom event: {kind}"),
        }
    }
}

// ─── Event Record ────────────────────────────────────────────────────────────

/// A timestamped, uniquely identified event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Unique event ID.
    pub id: Uuid,
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
    /// The event payload.
    pub event: FleetEvent,
    /// Optional source node.
    pub source_node: Option<String>,
    /// Optional trace ID for correlation.
    pub trace_id: Option<String>,
}

impl EventRecord {
    /// Create a new event record with auto-generated ID and current timestamp.
    pub fn new(event: FleetEvent) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            event,
            source_node: None,
            trace_id: None,
        }
    }

    /// Builder: set source node.
    pub fn with_source(mut self, node: impl Into<String>) -> Self {
        self.source_node = Some(node.into());
        self
    }

    /// Builder: set trace ID.
    pub fn with_trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }
}

// ─── Event Sink Trait ────────────────────────────────────────────────────────

/// Trait for event consumers.
///
/// Implementations can store events, forward them, trigger alerts, etc.
pub trait EventSink: Send + Sync {
    /// Receive an event record.
    fn receive(&self, record: &EventRecord);
}

// ─── In-Memory Sink ──────────────────────────────────────────────────────────

/// Simple in-memory event store with bounded capacity.
///
/// When the buffer is full, the oldest events are evicted (ring-buffer style).
#[derive(Debug, Clone)]
pub struct InMemoryEventSink {
    buffer: Arc<RwLock<Vec<EventRecord>>>,
    capacity: usize,
}

impl InMemoryEventSink {
    /// Create a new in-memory sink with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Arc::new(RwLock::new(Vec::with_capacity(capacity))),
            capacity,
        }
    }

    /// Blocking receive — for use from sync contexts or the `EventSink` trait.
    pub fn receive_sync(&self, record: &EventRecord) {
        // Use try_write to avoid blocking; drop oldest if full.
        if let Ok(mut buf) = self.buffer.try_write() {
            if buf.len() >= self.capacity {
                buf.remove(0);
            }
            buf.push(record.clone());
        }
    }

    /// Async: get all stored events.
    pub async fn events(&self) -> Vec<EventRecord> {
        self.buffer.read().await.clone()
    }

    /// Async: get events since a given timestamp.
    pub async fn events_since(&self, since: DateTime<Utc>) -> Vec<EventRecord> {
        self.buffer
            .read()
            .await
            .iter()
            .filter(|r| r.timestamp >= since)
            .cloned()
            .collect()
    }

    /// Async: get the last N events.
    pub async fn recent(&self, n: usize) -> Vec<EventRecord> {
        let buf = self.buffer.read().await;
        let start = buf.len().saturating_sub(n);
        buf[start..].to_vec()
    }

    /// Async: count of stored events.
    pub async fn len(&self) -> usize {
        self.buffer.read().await.len()
    }

    /// Async: is the sink empty?
    pub async fn is_empty(&self) -> bool {
        self.buffer.read().await.is_empty()
    }
}

impl EventSink for InMemoryEventSink {
    fn receive(&self, record: &EventRecord) {
        self.receive_sync(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_summary() {
        let e = FleetEvent::NodeOnline {
            node: "taylor".into(),
        };
        assert_eq!(e.summary(), "Node taylor came online");
    }

    #[test]
    fn test_event_record_builder() {
        let rec = EventRecord::new(FleetEvent::FleetStartup)
            .with_source("taylor")
            .with_trace("abc-123");
        assert_eq!(rec.source_node.as_deref(), Some("taylor"));
        assert_eq!(rec.trace_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn test_in_memory_sink_capacity() {
        let sink = InMemoryEventSink::new(2);
        sink.receive_sync(&EventRecord::new(FleetEvent::FleetStartup));
        sink.receive_sync(&EventRecord::new(FleetEvent::FleetShutdown));
        sink.receive_sync(&EventRecord::new(FleetEvent::NodeOnline {
            node: "x".into(),
        }));
        // Oldest should have been evicted — only 2 remain.
        let buf = sink.buffer.try_read().unwrap();
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn test_event_json_roundtrip() {
        let rec = EventRecord::new(FleetEvent::TierEscalation {
            from_tier: 1,
            to_tier: 2,
            reason: "model busy".into(),
        });
        let json = serde_json::to_string(&rec).unwrap();
        let back: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec.id, back.id);
    }
}
