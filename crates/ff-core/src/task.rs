use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::Result;

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

/// Insert a task lifecycle event into the transactional outbox.
///
/// This is meant to be called inside an existing Postgres transaction so the
/// outbox row is committed atomically with the `fleet_tasks` state change that
/// produced it. The caller is responsible for beginning and committing `tx`.
pub async fn publish_task_notification(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_id: Uuid,
    event_type: &str,
    payload: &Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_notification_outbox (task_id, event_type, payload)
         VALUES ($1, $2, $3)",
    )
    .bind(task_id)
    .bind(event_type)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
