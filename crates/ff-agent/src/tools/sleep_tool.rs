//! Sleep tool — pause execution for a specified duration.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct SleepTool;

#[async_trait]
impl AgentTool for SleepTool {
    fn name(&self) -> &str { "Sleep" }

    fn description(&self) -> &str {
        "Pause execution for a specified number of seconds. Use sparingly — prefer polling or event-driven approaches."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "description": "Number of seconds to sleep (max 300)"
                }
            },
            "required": ["seconds"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let seconds = input
            .get("seconds")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .min(300.0)
            .max(0.0);

        tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
        AgentToolResult::ok(format!("Slept for {seconds:.1} seconds"))
    }
}
