use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskKind {
    ShellCommand {
        command: String,
        timeout_secs: Option<u64>,
    },
    ModelInference {
        model: Option<String>,
        prompt: String,
        max_tokens: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub kind: AgentTaskKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: Uuid,
    pub success: bool,
    pub output: String,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: u64,
}
