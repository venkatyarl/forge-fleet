//! This module holds the ff-agent's in-memory shared runtime state.
use chrono::{DateTime, Utc};
use ff_core::{ActivityLevel, AgentTask, WorkerRole};
use ff_discovery::{HardwareProfile, HealthSnapshot};
use serde::Serialize;
use std::time::Instant;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type SharedState = Arc<RwLock<AgentState>>;

/// Bookkeeping for a build shell-command currently executing on this node.
///
/// The executor registers one of these when it starts a build and removes it
/// when the build finishes. The background build-timeout monitor scans these,
/// and when `started_at.elapsed()` exceeds the configured `max_build_duration`
/// it fires `cancel` — the executor is `select!`ing on that token and drops the
/// child process (killed via `kill_on_drop`).
#[derive(Debug)]
pub struct BuildWatch {
    pub started_at: Instant,
    pub cancel: CancellationToken,
}

#[derive(Debug)]
pub struct AgentState {
    pub node_id: String,
    pub role: WorkerRole,
    pub started_at: DateTime<Utc>,
    pub hardware: HardwareProfile,
    pub last_health: Option<HealthSnapshot>,
    pub activity_level: ActivityLevel,
    pub yield_resources: bool,
    pub active_tasks: HashMap<Uuid, AgentTask>,
    /// Builds currently running on this node, keyed by task id. Populated by
    /// the executor and consumed by the build-timeout monitor.
    pub build_watches: HashMap<Uuid, BuildWatch>,
    pub running_models: Vec<String>,
}

impl AgentState {
    pub fn new(node_id: String, hardware: HardwareProfile) -> Self {
        Self {
            node_id,
            role: WorkerRole::Worker,
            started_at: Utc::now(),
            hardware,
            last_health: None,
            activity_level: ActivityLevel::Idle,
            yield_resources: false,
            active_tasks: HashMap::new(),
            build_watches: HashMap::new(),
            running_models: vec![],
        }
    }

    pub fn to_status(&self) -> AgentStatus {
        AgentStatus {
            node_id: self.node_id.clone(),
            role: self.role,
            started_at: self.started_at,
            hardware: self.hardware.clone(),
            last_health: self.last_health.clone(),
            activity_level: self.activity_level,
            yield_resources: self.yield_resources,
            active_task_ids: self.active_tasks.keys().copied().collect(),
            running_models: self.running_models.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub node_id: String,
    pub role: WorkerRole,
    pub started_at: DateTime<Utc>,
    pub hardware: HardwareProfile,
    pub last_health: Option<HealthSnapshot>,
    pub activity_level: ActivityLevel,
    pub yield_resources: bool,
    pub active_task_ids: Vec<Uuid>,
    pub running_models: Vec<String>,
}
