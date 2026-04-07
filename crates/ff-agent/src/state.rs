use chrono::{DateTime, Utc};
use ff_core::{ActivityLevel, AgentTask, NodeRole};
use ff_discovery::{HardwareProfile, HealthSnapshot};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

pub type SharedState = Arc<RwLock<AgentState>>;

#[derive(Debug)]
pub struct AgentState {
    pub node_id: String,
    pub role: NodeRole,
    pub started_at: DateTime<Utc>,
    pub hardware: HardwareProfile,
    pub last_health: Option<HealthSnapshot>,
    pub activity_level: ActivityLevel,
    pub yield_resources: bool,
    pub active_tasks: HashMap<Uuid, AgentTask>,
    pub running_models: Vec<String>,
}

impl AgentState {
    pub fn new(node_id: String, hardware: HardwareProfile) -> Self {
        Self {
            node_id,
            role: NodeRole::Worker,
            started_at: Utc::now(),
            hardware,
            last_health: None,
            activity_level: ActivityLevel::Idle,
            yield_resources: false,
            active_tasks: HashMap::new(),
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
    pub role: NodeRole,
    pub started_at: DateTime<Utc>,
    pub hardware: HardwareProfile,
    pub last_health: Option<HealthSnapshot>,
    pub activity_level: ActivityLevel,
    pub yield_resources: bool,
    pub active_task_ids: Vec<Uuid>,
    pub running_models: Vec<String>,
}
