use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    #[default]
    Worker,
    BackupLeader,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivityLevel {
    Interactive,
    Assist,
    #[default]
    Idle,
    Protected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistrationAck {
    pub accepted: bool,
    pub role: WorkerRole,
    pub heartbeat_interval_secs: u64,
    pub reason: Option<String>,
}

impl Default for AgentRegistrationAck {
    fn default() -> Self {
        Self {
            accepted: true,
            role: WorkerRole::Worker,
            heartbeat_interval_secs: 15,
            reason: Some("leader_unreachable_using_local_defaults".to_string()),
        }
    }
}
