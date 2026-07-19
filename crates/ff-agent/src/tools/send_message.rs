//! SendMessage tool — send messages to other agents across fleet nodes.

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::Row;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct SendMessageTool {
    client: reqwest::Client,
}

impl Default for SendMessageTool {
    fn default() -> Self {
        Self {
            client: super::shared_http_client(),
        }
    }
}

#[async_trait]
impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to another agent on the fleet by node name, URL, or session ID. Use this to coordinate work between agents or dispatch tasks to specific fleet nodes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target agent: node name (e.g. 'marcus'), full URL (e.g. 'http://192.168.5.102:50002'), or session UUID"
                },
                "message": {
                    "type": "string",
                    "description": "The message content to send"
                }
            },
            "required": ["to", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let to = input
            .get("to")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        if to.is_empty() || message.is_empty() {
            return AgentToolResult::err("Both 'to' and 'message' are required");
        }

        // 'to' can be:
        // - a full URL like "http://192.168.5.102:50002"
        // - a node name like "marcus" (resolved via fleet fallback table)
        // - a session_id UUID
        let target_url = if to.starts_with("http://") || to.starts_with("https://") {
            format!("{}/agent/message", to.trim_end_matches('/'))
        } else {
            resolve_node_url(&to).await
        };

        let payload = json!({
            "from": ctx.session_id,
            "to": to,
            "message": message,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        match self.client.post(&target_url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                AgentToolResult::ok(format!("Message delivered to '{to}' ({})", resp.status()))
            }
            Ok(resp) => AgentToolResult::err(format!(
                "Delivery failed: {} returned {}",
                target_url,
                resp.status()
            )),
            Err(e) => AgentToolResult::err(format!("Failed to reach '{to}' at {target_url}: {e}")),
        }
    }
}

/// Resolve a node name to its agent HTTP endpoint URL.
/// Tries the DB first; falls back to a known fleet table when the DB is unavailable.
async fn resolve_node_url(name: &str) -> String {
    // Try DB first
    if let Ok(ip) = lookup_node_ip_from_db(name).await {
        return format!("http://{ip}:50002/agent/message");
    }
    // Fallback to known-good table
    let known: std::collections::HashMap<&str, &str> = [
        ("taylor", "192.168.5.100"),
        ("marcus", "192.168.5.102"),
        ("sophie", "192.168.5.103"),
        ("priya", "192.168.5.104"),
        ("james", "192.168.5.108"),
        ("logan", "192.168.5.111"),
        ("veronica", "192.168.5.112"),
        ("lily", "192.168.5.113"),
        ("duncan", "192.168.5.114"),
        ("aura", "192.168.5.110"),
    ]
    .into();
    if let Some(ip) = known.get(name.to_lowercase().as_str()) {
        format!("http://{ip}:50002/agent/message")
    } else {
        format!("http://{name}:50002/agent/message")
    }
}

async fn lookup_node_ip_from_db(name: &str) -> anyhow::Result<String> {
    let pool = crate::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("get_fleet_pool: {e}"))?;

    let row = sqlx::query("SELECT ip FROM fleet_workers WHERE name = $1")
        .bind(name)
        .fetch_one(&pool)
        .await?;

    Ok(row.try_get::<String, _>("ip")?)
}
