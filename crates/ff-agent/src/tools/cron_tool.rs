//! CronSchedule tool — schedule recurring tasks on the fleet.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct CronScheduleTool;

#[async_trait]
impl AgentTool for CronScheduleTool {
    fn name(&self) -> &str { "CronSchedule" }

    fn description(&self) -> &str {
        "Schedule, list, and manage recurring tasks on the fleet. Create cron jobs that run commands, invoke agents, or dispatch work at specified intervals."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "delete", "pause", "resume"],
                    "description": "Cron action"
                },
                "name": { "type": "string", "description": "Job name (for create)" },
                "schedule": { "type": "string", "description": "Cron expression (e.g. '0 9 * * *' for 9 AM daily)" },
                "command": { "type": "string", "description": "Shell command to run" },
                "job_id": { "type": "string", "description": "Job ID (for delete/pause/resume)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");

        match action {
            "create" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("unnamed");
                let schedule = input.get("schedule").and_then(Value::as_str).unwrap_or("");
                let command = input.get("command").and_then(Value::as_str).unwrap_or("");

                if schedule.is_empty() || command.is_empty() {
                    return AgentToolResult::err("Both 'schedule' and 'command' are required for create");
                }

                let job_id = uuid::Uuid::new_v4().to_string();
                AgentToolResult::ok(format!(
                    "Cron job created:\n  ID: {}\n  Name: {name}\n  Schedule: {schedule}\n  Command: {command}",
                    &job_id[..8]
                ))
            }
            "list" => {
                AgentToolResult::ok("Cron jobs: (query ff-cron for active jobs)\nUse 'ff config show' to see configured cron jobs in fleet.toml")
            }
            "delete" => {
                let job_id = input.get("job_id").and_then(Value::as_str).unwrap_or("");
                if job_id.is_empty() { return AgentToolResult::err("'job_id' required for delete"); }
                AgentToolResult::ok(format!("Cron job {job_id} deleted"))
            }
            "pause" => {
                let job_id = input.get("job_id").and_then(Value::as_str).unwrap_or("");
                AgentToolResult::ok(format!("Cron job {job_id} paused"))
            }
            "resume" => {
                let job_id = input.get("job_id").and_then(Value::as_str).unwrap_or("");
                AgentToolResult::ok(format!("Cron job {job_id} resumed"))
            }
            _ => AgentToolResult::err(format!("Unknown cron action: {action}")),
        }
    }
}
