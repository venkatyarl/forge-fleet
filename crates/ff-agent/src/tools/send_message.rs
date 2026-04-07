//! SendMessage tool — send messages between agents (placeholder for multi-agent coordination).

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct SendMessageTool;

#[async_trait]
impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to another running agent by name or ID. Use this to coordinate work between agents or to continue a previously spawned agent's conversation."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Agent name or ID to send the message to"
                },
                "message": {
                    "type": "string",
                    "description": "The message content to send"
                }
            },
            "required": ["to", "message"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let to = input.get("to").and_then(Value::as_str).unwrap_or("");
        let message = input.get("message").and_then(Value::as_str).unwrap_or("");

        if to.is_empty() || message.is_empty() {
            return AgentToolResult::err("Both 'to' and 'message' are required");
        }

        // TODO: Wire to actual agent session registry for inter-agent communication
        // For now, this is a placeholder that acknowledges the message
        AgentToolResult::ok(format!(
            "Message queued for agent '{to}'. Note: inter-agent messaging is pending full implementation."
        ))
    }
}
