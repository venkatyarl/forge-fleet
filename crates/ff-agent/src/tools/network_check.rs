//! NetworkCheck tool — fleet network diagnostics.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct NetworkCheckTool;

#[async_trait]
impl AgentTool for NetworkCheckTool {
    fn name(&self) -> &str { "NetworkCheck" }
    fn description(&self) -> &str { "Network diagnostics: ping hosts, DNS lookup, check port connectivity, and test fleet node reachability." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["ping", "dns", "port", "fleet"], "description": "Check type" },
                "host": { "type": "string", "description": "Host to check" },
                "port": { "type": "number", "description": "Port to check (for port action)" }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let host = input.get("host").and_then(Value::as_str).unwrap_or("");

        match action {
            "ping" => {
                if host.is_empty() { return AgentToolResult::err("'host' required for ping"); }
                match Command::new("ping").args(["-c", "3", "-W", "2", host]).output().await {
                    Ok(out) => AgentToolResult::ok(String::from_utf8_lossy(&out.stdout).to_string()),
                    Err(e) => AgentToolResult::err(format!("Ping failed: {e}")),
                }
            }
            "dns" => {
                if host.is_empty() { return AgentToolResult::err("'host' required for dns"); }
                match Command::new("host").arg(host).output().await {
                    Ok(out) => AgentToolResult::ok(String::from_utf8_lossy(&out.stdout).to_string()),
                    Err(_) => match Command::new("nslookup").arg(host).output().await {
                        Ok(out) => AgentToolResult::ok(String::from_utf8_lossy(&out.stdout).to_string()),
                        Err(e) => AgentToolResult::err(format!("DNS lookup failed: {e}")),
                    }
                }
            }
            "port" => {
                if host.is_empty() { return AgentToolResult::err("'host' required"); }
                let port = input.get("port").and_then(Value::as_u64).unwrap_or(80);
                let addr = format!("{host}:{port}");
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect(&addr),
                ).await {
                    Ok(Ok(_)) => AgentToolResult::ok(format!("{addr} — OPEN")),
                    _ => AgentToolResult::ok(format!("{addr} — CLOSED/UNREACHABLE")),
                }
            }
            "fleet" => {
                // Load fleet topology from Postgres (no hardcoded node list).
                let snapshot = match crate::fleet_info::fetch_snapshot().await {
                    Ok(s) => s,
                    Err(e) => return AgentToolResult::err(format!("Failed to load fleet from database: {e}")),
                };
                if snapshot.nodes.is_empty() {
                    return AgentToolResult::ok(
                        "No fleet nodes registered in the database.".to_string(),
                    );
                }

                let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap_or_default();
                let mut results = Vec::new();
                for node in &snapshot.nodes {
                    // Prefer a model-specific port if one exists; otherwise default to 51000.
                    let node_models: Vec<&ff_db::FleetModelRow> = snapshot
                        .models
                        .iter()
                        .filter(|m| m.node_name == node.name)
                        .collect();
                    let ports: Vec<u16> = if node_models.is_empty() {
                        vec![51000]
                    } else {
                        node_models.iter().map(|m| m.port as u16).collect()
                    };
                    for port in ports {
                        let url = format!("http://{}:{}/health", node.ip, port);
                        let status = match client.get(&url).send().await {
                            Ok(r) if r.status().is_success() => "ONLINE",
                            _ => "OFFLINE",
                        };
                        results.push(format!(
                            "{name:<10} {ip}:{port:<5} {status}",
                            name = node.name,
                            ip = node.ip,
                        ));
                    }
                }
                AgentToolResult::ok(results.join("\n"))
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}
