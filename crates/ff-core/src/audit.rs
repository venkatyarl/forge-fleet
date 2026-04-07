//! Audit trail system for ForgeFleet.
//!
//! Records every significant fleet event — config changes, model lifecycle,
//! node membership, deployments, backups, and task assignments.
//!
//! # Architecture
//!
//! [`AuditLogger`] delegates persistence to the [`AuditStore`] trait, keeping
//! `ff-core` free from direct database dependencies. The implementation
//! (e.g., `ff-db` SQLite store) is injected at construction time.
//!
//! ```rust,no_run
//! use ff_core::audit::{AuditLogger, AuditAction};
//! # use std::sync::Arc;
//! # fn example(store: Arc<dyn ff_core::audit::AuditStore>) {
//! let logger = AuditLogger::new(store);
//! logger.log_config_change("venkat", "fleet.name", Some("old"), "new", None).unwrap();
//! # }
//! ```

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{ForgeFleetError, Result};

// ─── Action Enum ─────────────────────────────────────────────────────────────

/// Categorized fleet actions for the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    ConfigChanged,
    ModelStarted,
    ModelStopped,
    NodeJoined,
    NodeLeft,
    LeaderChanged,
    UpdateStarted,
    UpdateCompleted,
    BackupCreated,
    TaskAssigned,
    TaskCompleted,
}

impl AuditAction {
    /// Wire-format string matching `audit_log.event_type` in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConfigChanged => "config_changed",
            Self::ModelStarted => "model_started",
            Self::ModelStopped => "model_stopped",
            Self::NodeJoined => "node_joined",
            Self::NodeLeft => "node_left",
            Self::LeaderChanged => "leader_changed",
            Self::UpdateStarted => "update_started",
            Self::UpdateCompleted => "update_completed",
            Self::BackupCreated => "backup_created",
            Self::TaskAssigned => "task_assigned",
            Self::TaskCompleted => "task_completed",
        }
    }

    /// All known audit actions (useful for UI dropdowns or validation).
    pub fn all() -> &'static [AuditAction] {
        &[
            Self::ConfigChanged,
            Self::ModelStarted,
            Self::ModelStopped,
            Self::NodeJoined,
            Self::NodeLeft,
            Self::LeaderChanged,
            Self::UpdateStarted,
            Self::UpdateCompleted,
            Self::BackupCreated,
            Self::TaskAssigned,
            Self::TaskCompleted,
        ]
    }

    /// Parse from the wire-format string stored in the database.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "config_changed" => Some(Self::ConfigChanged),
            "model_started" => Some(Self::ModelStarted),
            "model_stopped" => Some(Self::ModelStopped),
            "node_joined" => Some(Self::NodeJoined),
            "node_left" => Some(Self::NodeLeft),
            "leader_changed" => Some(Self::LeaderChanged),
            "update_started" => Some(Self::UpdateStarted),
            "update_completed" => Some(Self::UpdateCompleted),
            "backup_created" => Some(Self::BackupCreated),
            "task_assigned" => Some(Self::TaskAssigned),
            "task_completed" => Some(Self::TaskCompleted),
            _ => None,
        }
    }
}

impl fmt::Display for AuditAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Event ───────────────────────────────────────────────────────────────────

/// A single audit event.
///
/// Maps to the `audit_log` table. The `source_ip` field is stored in the
/// `node_name` column; if both a node name and IP are needed, the IP can
/// also be placed in `details`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Database row ID (`0` before persistence).
    pub id: i64,
    /// When the event occurred (UTC).
    pub timestamp: DateTime<Utc>,
    /// Who triggered the event — node name, user ID, or `"system"`.
    pub actor: String,
    /// What happened.
    pub action: AuditAction,
    /// What was affected — model slug, node name, config key, task ID, etc.
    pub target: Option<String>,
    /// Structured details (serialized as JSON).
    pub details: serde_json::Value,
    /// Originating IP address or node name.
    pub source_ip: Option<String>,
}

impl AuditEvent {
    /// Construct a new event. `id` defaults to 0 (assigned by the store on insert).
    pub fn new(
        actor: impl Into<String>,
        action: AuditAction,
        target: Option<String>,
        details: serde_json::Value,
        source_ip: Option<String>,
    ) -> Self {
        Self {
            id: 0,
            timestamp: Utc::now(),
            actor: actor.into(),
            action,
            target,
            details,
            source_ip,
        }
    }
}

// ─── Store Trait ──────────────────────────────────────────────────────────────

/// Persistence backend for audit events.
///
/// Implement this for SQLite (`ff-db`), Postgres, or an in-memory store for testing.
/// All methods are synchronous since `ff-db` uses synchronous `rusqlite`.
pub trait AuditStore: Send + Sync {
    /// Persist an event and return its assigned row ID.
    fn insert(&self, event: &AuditEvent) -> Result<i64>;

    /// Retrieve the most recent `limit` events, newest first.
    fn recent_events(&self, limit: u32) -> Result<Vec<AuditEvent>>;

    /// Filter events by action type, newest first.
    fn events_by_action(&self, action: AuditAction, limit: u32) -> Result<Vec<AuditEvent>>;

    /// Filter events by actor, newest first.
    fn events_by_actor(&self, actor: &str, limit: u32) -> Result<Vec<AuditEvent>>;

    /// Filter events within an ISO-8601 time range, newest first.
    fn events_in_range(&self, from: &str, to: &str, limit: u32) -> Result<Vec<AuditEvent>>;
}

// ─── Logger ──────────────────────────────────────────────────────────────────

/// High-level audit logger with typed convenience methods.
///
/// Thread-safe (`Clone + Send + Sync`) — share freely across subsystems.
///
/// # Usage
///
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use ff_core::audit::*;
/// # fn example(store: Arc<dyn AuditStore>) {
/// let logger = AuditLogger::new(store);
///
/// // Log a config change
/// logger.log_config_change("venkat", "fleet.name", Some("old"), "new-fleet", None).unwrap();
///
/// // Log a model starting
/// logger.log_model_event(
///     AuditAction::ModelStarted, "system", "qwen-32b", "taylor", None, None,
/// ).unwrap();
///
/// // Query recent events
/// let recent = logger.recent_events(50).unwrap();
/// # }
/// ```
#[derive(Clone)]
pub struct AuditLogger {
    store: Arc<dyn AuditStore>,
}

impl AuditLogger {
    /// Create a logger backed by the given store.
    pub fn new(store: Arc<dyn AuditStore>) -> Self {
        Self { store }
    }

    /// Access the underlying store (for advanced queries).
    pub fn store(&self) -> &dyn AuditStore {
        self.store.as_ref()
    }

    // ─── Convenience writes ──────────────────────────────────────

    /// Log a configuration change.
    pub fn log_config_change(
        &self,
        actor: &str,
        key: &str,
        old_value: Option<&str>,
        new_value: &str,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let details = serde_json::json!({
            "key": key,
            "old_value": old_value,
            "new_value": new_value,
        });
        let event = AuditEvent::new(
            actor,
            AuditAction::ConfigChanged,
            Some(key.to_string()),
            details,
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, actor, key, "audit: config changed");
        Ok(id)
    }

    /// Log a model lifecycle event (started / stopped).
    pub fn log_model_event(
        &self,
        action: AuditAction,
        actor: &str,
        model: &str,
        node: &str,
        extra: Option<serde_json::Value>,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let details = serde_json::json!({
            "model": model,
            "node": node,
            "extra": extra,
        });
        let event = AuditEvent::new(
            actor,
            action,
            Some(model.to_string()),
            details,
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, actor, model, node, %action, "audit: model event");
        Ok(id)
    }

    /// Log a node membership event (joined / left / leader changed).
    pub fn log_node_event(
        &self,
        action: AuditAction,
        actor: &str,
        node: &str,
        details: Option<serde_json::Value>,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let event = AuditEvent::new(
            actor,
            action,
            Some(node.to_string()),
            details.unwrap_or(serde_json::json!({})),
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, actor, node, %action, "audit: node event");
        Ok(id)
    }

    /// Log an update / deployment event (started / completed).
    pub fn log_update_event(
        &self,
        action: AuditAction,
        actor: &str,
        target: &str,
        version: Option<&str>,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let details = serde_json::json!({
            "target": target,
            "version": version,
        });
        let event = AuditEvent::new(
            actor,
            action,
            Some(target.to_string()),
            details,
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, actor, target, %action, "audit: update event");
        Ok(id)
    }

    /// Log a task lifecycle event (assigned / completed).
    pub fn log_task_event(
        &self,
        action: AuditAction,
        actor: &str,
        task_id: &str,
        node: Option<&str>,
        details: Option<serde_json::Value>,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let mut d = details.unwrap_or(serde_json::json!({}));
        if let Some(n) = node {
            d["assigned_node"] = serde_json::json!(n);
        }
        let event = AuditEvent::new(
            actor,
            action,
            Some(task_id.to_string()),
            d,
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, actor, task_id, %action, "audit: task event");
        Ok(id)
    }

    /// Log a backup creation event.
    pub fn log_backup_event(
        &self,
        actor: &str,
        path: &str,
        size_bytes: u64,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let details = serde_json::json!({
            "path": path,
            "size_bytes": size_bytes,
        });
        let event = AuditEvent::new(
            actor,
            AuditAction::BackupCreated,
            Some(path.to_string()),
            details,
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, actor, path, "audit: backup created");
        Ok(id)
    }

    /// Log a leader election change.
    pub fn log_leader_changed(
        &self,
        new_leader: &str,
        reason: Option<&str>,
        source_ip: Option<&str>,
    ) -> Result<i64> {
        let details = serde_json::json!({
            "new_leader": new_leader,
            "reason": reason,
        });
        let event = AuditEvent::new(
            "system",
            AuditAction::LeaderChanged,
            Some(new_leader.to_string()),
            details,
            source_ip.map(String::from),
        );
        let id = self.store.insert(&event)?;
        info!(id, new_leader, "audit: leader changed");
        Ok(id)
    }

    // ─── Query delegates ─────────────────────────────────────────

    /// Most recent events, newest first.
    pub fn recent_events(&self, limit: u32) -> Result<Vec<AuditEvent>> {
        self.store.recent_events(limit)
    }

    /// Events matching a specific action.
    pub fn events_by_action(&self, action: AuditAction, limit: u32) -> Result<Vec<AuditEvent>> {
        self.store.events_by_action(action, limit)
    }

    /// Events triggered by a specific actor.
    pub fn events_by_actor(&self, actor: &str, limit: u32) -> Result<Vec<AuditEvent>> {
        self.store.events_by_actor(actor, limit)
    }

    /// Events in a time range (ISO 8601 bounds).
    pub fn events_in_range(&self, from: &str, to: &str, limit: u32) -> Result<Vec<AuditEvent>> {
        self.store.events_in_range(from, to, limit)
    }
}

// ─── In-Memory Store (for testing) ───────────────────────────────────────────

/// Simple in-memory audit store, useful for unit tests.
///
/// Not intended for production use — events are lost on drop.
pub struct InMemoryAuditStore {
    events: std::sync::Mutex<Vec<AuditEvent>>,
    next_id: std::sync::Mutex<i64>,
}

impl InMemoryAuditStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            next_id: std::sync::Mutex::new(1),
        }
    }
}

impl Default for InMemoryAuditStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditStore for InMemoryAuditStore {
    fn insert(&self, event: &AuditEvent) -> Result<i64> {
        let mut events = self
            .events
            .lock()
            .map_err(|e| ForgeFleetError::Internal(format!("audit store lock poisoned: {e}")))?;
        let mut next_id = self
            .next_id
            .lock()
            .map_err(|e| ForgeFleetError::Internal(format!("audit store lock poisoned: {e}")))?;
        let id = *next_id;
        *next_id += 1;
        let mut stored = event.clone();
        stored.id = id;
        events.push(stored);
        Ok(id)
    }

    fn recent_events(&self, limit: u32) -> Result<Vec<AuditEvent>> {
        let events = self
            .events
            .lock()
            .map_err(|e| ForgeFleetError::Internal(format!("audit store lock poisoned: {e}")))?;
        let mut sorted = events.clone();
        sorted.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(sorted.into_iter().take(limit as usize).collect())
    }

    fn events_by_action(&self, action: AuditAction, limit: u32) -> Result<Vec<AuditEvent>> {
        let events = self
            .events
            .lock()
            .map_err(|e| ForgeFleetError::Internal(format!("audit store lock poisoned: {e}")))?;
        Ok(events
            .iter()
            .filter(|e| e.action == action)
            .rev()
            .take(limit as usize)
            .cloned()
            .collect())
    }

    fn events_by_actor(&self, actor: &str, limit: u32) -> Result<Vec<AuditEvent>> {
        let events = self
            .events
            .lock()
            .map_err(|e| ForgeFleetError::Internal(format!("audit store lock poisoned: {e}")))?;
        Ok(events
            .iter()
            .filter(|e| e.actor == actor)
            .rev()
            .take(limit as usize)
            .cloned()
            .collect())
    }

    fn events_in_range(&self, from: &str, to: &str, limit: u32) -> Result<Vec<AuditEvent>> {
        let from_dt = DateTime::parse_from_rfc3339(from)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let to_dt = DateTime::parse_from_rfc3339(to)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let events = self
            .events
            .lock()
            .map_err(|e| ForgeFleetError::Internal(format!("audit store lock poisoned: {e}")))?;
        Ok(events
            .iter()
            .filter(|e| e.timestamp >= from_dt && e.timestamp <= to_dt)
            .rev()
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_action_roundtrip() {
        for action in AuditAction::all() {
            let s = action.as_str();
            let parsed = AuditAction::parse(s);
            assert_eq!(parsed, Some(*action), "roundtrip failed for {s}");
        }
        assert_eq!(AuditAction::parse("unknown_thing"), None);
    }

    #[test]
    fn test_audit_action_display() {
        assert_eq!(AuditAction::ConfigChanged.to_string(), "config_changed");
        assert_eq!(AuditAction::LeaderChanged.to_string(), "leader_changed");
    }

    #[test]
    fn test_audit_event_serde() {
        let event = AuditEvent::new(
            "taylor",
            AuditAction::ModelStarted,
            Some("qwen-32b".to_string()),
            serde_json::json!({"port": 51800}),
            Some("192.168.5.100".to_string()),
        );
        let json = serde_json::to_string(&event).unwrap();
        let back: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.actor, "taylor");
        assert_eq!(back.action, AuditAction::ModelStarted);
        assert_eq!(back.target.as_deref(), Some("qwen-32b"));
        assert_eq!(back.source_ip.as_deref(), Some("192.168.5.100"));
    }

    #[test]
    fn test_logger_config_change() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        let id = logger
            .log_config_change("venkat", "fleet.name", Some("old"), "new-fleet", None)
            .unwrap();
        assert!(id > 0);

        let events = logger.recent_events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].action, AuditAction::ConfigChanged);
        assert_eq!(events[0].actor, "venkat");
        assert_eq!(events[0].target.as_deref(), Some("fleet.name"));
    }

    #[test]
    fn test_logger_model_event() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        logger
            .log_model_event(
                AuditAction::ModelStarted,
                "system",
                "qwen-32b",
                "taylor",
                None,
                Some("192.168.5.100"),
            )
            .unwrap();

        logger
            .log_model_event(
                AuditAction::ModelStopped,
                "system",
                "qwen-32b",
                "taylor",
                None,
                None,
            )
            .unwrap();

        let started = logger
            .events_by_action(AuditAction::ModelStarted, 10)
            .unwrap();
        assert_eq!(started.len(), 1);

        let by_system = logger.events_by_actor("system", 10).unwrap();
        assert_eq!(by_system.len(), 2);
    }

    #[test]
    fn test_logger_node_events() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        logger
            .log_node_event(AuditAction::NodeJoined, "james", "james", None, None)
            .unwrap();
        logger
            .log_node_event(
                AuditAction::LeaderChanged,
                "system",
                "taylor",
                Some(serde_json::json!({"reason": "preferred"})),
                None,
            )
            .unwrap();

        let all = logger.recent_events(10).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_logger_update_event() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        logger
            .log_update_event(
                AuditAction::UpdateStarted,
                "system",
                "forgefleetd",
                Some("0.2.0"),
                None,
            )
            .unwrap();
        logger
            .log_update_event(
                AuditAction::UpdateCompleted,
                "system",
                "forgefleetd",
                Some("0.2.0"),
                None,
            )
            .unwrap();

        let updates = logger
            .events_by_action(AuditAction::UpdateStarted, 10)
            .unwrap();
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn test_logger_task_event() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        logger
            .log_task_event(
                AuditAction::TaskAssigned,
                "orchestrator",
                "task-001",
                Some("james"),
                None,
                None,
            )
            .unwrap();

        let tasks = logger
            .events_by_action(AuditAction::TaskAssigned, 10)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].details["assigned_node"] == "james");
    }

    #[test]
    fn test_logger_backup_event() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        logger
            .log_backup_event("system", "/backups/fleet-2026-04.db", 1024 * 1024, None)
            .unwrap();

        let backups = logger
            .events_by_action(AuditAction::BackupCreated, 10)
            .unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].details["size_bytes"], 1048576);
    }

    #[test]
    fn test_logger_leader_changed() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        logger
            .log_leader_changed("taylor", Some("highest priority"), None)
            .unwrap();

        let events = logger
            .events_by_action(AuditAction::LeaderChanged, 10)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].target.as_deref(), Some("taylor"));
    }

    #[test]
    fn test_events_in_range() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        // Log some events
        logger
            .log_config_change("a", "k1", None, "v1", None)
            .unwrap();

        let now = Utc::now();
        let from = (now - chrono::Duration::seconds(5)).to_rfc3339();
        let to = (now + chrono::Duration::seconds(5)).to_rfc3339();

        let in_range = logger.events_in_range(&from, &to, 10).unwrap();
        assert_eq!(in_range.len(), 1);

        // Way in the past — should find nothing
        let old_from = "2020-01-01T00:00:00Z";
        let old_to = "2020-01-02T00:00:00Z";
        let empty = logger.events_in_range(old_from, old_to, 10).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_recent_events_ordering() {
        let store = Arc::new(InMemoryAuditStore::new());
        let logger = AuditLogger::new(store);

        for i in 0..5 {
            logger
                .log_config_change("sys", &format!("key-{i}"), None, "v", None)
                .unwrap();
        }

        let recent = logger.recent_events(3).unwrap();
        assert_eq!(recent.len(), 3);
        // Newest first — IDs should be descending
        assert!(recent[0].id > recent[1].id);
        assert!(recent[1].id > recent[2].id);
    }
}
