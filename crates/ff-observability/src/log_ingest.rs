//! Log ingestion — ingest and buffer structured logs from fleet nodes and agents.
//!
//! Each node or agent can push log entries to a central [`LogIngestor`] via
//! async channels. The ingestor stores them in a bounded ring buffer for
//! querying by the dashboard, forwarding to external systems, or correlation
//! with alerts.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

// ─── Log Level ───────────────────────────────────────────────────────────────

/// Severity level for ingested log entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trace => write!(f, "TRACE"),
            Self::Debug => write!(f, "DEBUG"),
            Self::Info => write!(f, "INFO"),
            Self::Warn => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

impl LogLevel {
    /// Parse from a string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "trace" => Self::Trace,
            "debug" => Self::Debug,
            "info" => Self::Info,
            "warn" | "warning" => Self::Warn,
            "error" | "err" | "fatal" => Self::Error,
            _ => Self::Info,
        }
    }
}

// ─── Log Entry ───────────────────────────────────────────────────────────────

/// A single structured log entry from a node or agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unique entry ID.
    pub id: Uuid,
    /// When the log was emitted at the source.
    pub timestamp: DateTime<Utc>,
    /// When the log was received by the ingestor.
    pub received_at: DateTime<Utc>,
    /// Source node name.
    pub node: String,
    /// Source component (e.g. "ff-agent", "ff-runtime", "llama-server").
    pub component: String,
    /// Log level.
    pub level: LogLevel,
    /// Log message.
    pub message: String,
    /// Optional structured fields (key=value pairs).
    #[serde(default)]
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Optional trace/span ID for correlation.
    pub trace_id: Option<String>,
    /// Optional span ID.
    pub span_id: Option<String>,
}

impl LogEntry {
    /// Create a new log entry with auto-generated ID and current receive time.
    pub fn new(
        node: impl Into<String>,
        component: impl Into<String>,
        level: LogLevel,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            received_at: Utc::now(),
            node: node.into(),
            component: component.into(),
            level,
            message: message.into(),
            fields: serde_json::Map::new(),
            trace_id: None,
            span_id: None,
        }
    }

    /// Builder: set a structured field.
    pub fn with_field(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// Builder: set trace ID.
    pub fn with_trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Builder: set original timestamp (from the source node).
    pub fn with_timestamp(mut self, ts: DateTime<Utc>) -> Self {
        self.timestamp = ts;
        self
    }
}

// ─── Log Buffer ──────────────────────────────────────────────────────────────

/// Bounded ring buffer for log entries.
#[derive(Debug, Clone)]
pub struct LogBuffer {
    entries: Arc<RwLock<Vec<LogEntry>>>,
    capacity: usize,
}

impl LogBuffer {
    /// Create a new buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(Vec::with_capacity(capacity))),
            capacity,
        }
    }

    /// Push a log entry, evicting the oldest if at capacity.
    pub async fn push(&self, entry: LogEntry) {
        let mut buf = self.entries.write().await;
        if buf.len() >= self.capacity {
            buf.remove(0);
        }
        buf.push(entry);
    }

    /// Push synchronously (best-effort, non-blocking).
    pub fn push_sync(&self, entry: LogEntry) {
        if let Ok(mut buf) = self.entries.try_write() {
            if buf.len() >= self.capacity {
                buf.remove(0);
            }
            buf.push(entry);
        }
    }

    /// Query entries by node.
    pub async fn by_node(&self, node: &str) -> Vec<LogEntry> {
        self.entries
            .read()
            .await
            .iter()
            .filter(|e| e.node == node)
            .cloned()
            .collect()
    }

    /// Query entries at or above a given level.
    pub async fn by_level(&self, min_level: LogLevel) -> Vec<LogEntry> {
        self.entries
            .read()
            .await
            .iter()
            .filter(|e| e.level >= min_level)
            .cloned()
            .collect()
    }

    /// Query entries since a given timestamp.
    pub async fn since(&self, ts: DateTime<Utc>) -> Vec<LogEntry> {
        self.entries
            .read()
            .await
            .iter()
            .filter(|e| e.timestamp >= ts)
            .cloned()
            .collect()
    }

    /// Get the most recent N entries.
    pub async fn recent(&self, n: usize) -> Vec<LogEntry> {
        let buf = self.entries.read().await;
        let start = buf.len().saturating_sub(n);
        buf[start..].to_vec()
    }

    /// Search entries whose message contains a substring.
    pub async fn search(&self, query: &str) -> Vec<LogEntry> {
        let q = query.to_lowercase();
        self.entries
            .read()
            .await
            .iter()
            .filter(|e| e.message.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }

    /// Total number of entries in the buffer.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Whether the buffer is empty.
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }
}

// ─── Log Ingestor ────────────────────────────────────────────────────────────

/// Central log ingestor that receives logs from all fleet nodes.
///
/// Maintains a global buffer plus per-node buffers for fast lookups.
#[derive(Debug, Clone)]
pub struct LogIngestor {
    /// Global log buffer.
    pub global: LogBuffer,
    /// Per-node log buffers (key = node name).
    per_node: Arc<DashMap<String, LogBuffer>>,
    /// Capacity for per-node buffers.
    per_node_capacity: usize,
}

impl LogIngestor {
    /// Create a new ingestor.
    ///
    /// - `global_capacity`: max entries in the global buffer.
    /// - `per_node_capacity`: max entries per node buffer.
    pub fn new(global_capacity: usize, per_node_capacity: usize) -> Self {
        Self {
            global: LogBuffer::new(global_capacity),
            per_node: Arc::new(DashMap::new()),
            per_node_capacity,
        }
    }

    /// Ingest a log entry.
    pub async fn ingest(&self, entry: LogEntry) {
        // Store in per-node buffer.
        let node_name = entry.node.clone();
        let node_buf = self
            .per_node
            .entry(node_name)
            .or_insert_with(|| LogBuffer::new(self.per_node_capacity))
            .clone();
        node_buf.push(entry.clone()).await;

        // Store in global buffer.
        self.global.push(entry).await;
    }

    /// Ingest synchronously (best-effort).
    pub fn ingest_sync(&self, entry: LogEntry) {
        let node_name = entry.node.clone();
        let node_buf = self
            .per_node
            .entry(node_name)
            .or_insert_with(|| LogBuffer::new(self.per_node_capacity))
            .clone();
        node_buf.push_sync(entry.clone());
        self.global.push_sync(entry);
    }

    /// Get logs for a specific node.
    pub async fn logs_for_node(&self, node: &str) -> Vec<LogEntry> {
        match self.per_node.get(node) {
            Some(buf) => buf.recent(1000).await,
            None => Vec::new(),
        }
    }

    /// Get recent errors across all nodes.
    pub async fn recent_errors(&self, n: usize) -> Vec<LogEntry> {
        let all = self.global.by_level(LogLevel::Error).await;
        let start = all.len().saturating_sub(n);
        all[start..].to_vec()
    }

    /// List all node names that have sent logs.
    pub fn known_nodes(&self) -> Vec<String> {
        self.per_node.iter().map(|e| e.key().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn test_log_level_parse() {
        assert_eq!(LogLevel::from_str_loose("WARNING"), LogLevel::Warn);
        assert_eq!(LogLevel::from_str_loose("fatal"), LogLevel::Error);
        assert_eq!(LogLevel::from_str_loose("junk"), LogLevel::Info);
    }

    #[test]
    fn test_log_entry_builder() {
        let entry = LogEntry::new("taylor", "ff-agent", LogLevel::Info, "booted")
            .with_field("version", serde_json::Value::String("0.1.0".into()))
            .with_trace("abc-123");
        assert_eq!(entry.node, "taylor");
        assert_eq!(entry.trace_id.as_deref(), Some("abc-123"));
        assert!(entry.fields.contains_key("version"));
    }

    #[tokio::test]
    async fn test_log_buffer_capacity() {
        let buf = LogBuffer::new(2);
        buf.push(LogEntry::new("a", "c", LogLevel::Info, "msg1"))
            .await;
        buf.push(LogEntry::new("a", "c", LogLevel::Info, "msg2"))
            .await;
        buf.push(LogEntry::new("a", "c", LogLevel::Info, "msg3"))
            .await;
        assert_eq!(buf.len().await, 2);
        let recent = buf.recent(10).await;
        assert_eq!(recent[0].message, "msg2");
    }

    #[tokio::test]
    async fn test_ingestor_per_node() {
        let ingestor = LogIngestor::new(100, 50);
        ingestor
            .ingest(LogEntry::new("taylor", "ff-agent", LogLevel::Info, "hello"))
            .await;
        ingestor
            .ingest(LogEntry::new("james", "ff-agent", LogLevel::Warn, "hot"))
            .await;

        let taylor_logs = ingestor.logs_for_node("taylor").await;
        assert_eq!(taylor_logs.len(), 1);

        let nodes = ingestor.known_nodes();
        assert_eq!(nodes.len(), 2);
    }
}
