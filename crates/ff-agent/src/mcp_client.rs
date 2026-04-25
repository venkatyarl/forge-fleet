//! MCP Client — connect to external MCP servers and invoke their tools.
//!
//! ForgeFleet's ff-mcp crate is an MCP *server*. This module makes the agent
//! an MCP *client* that can discover and use tools from external MCP servers
//! (like filesystem, database, GitHub, Slack, etc.).

use std::collections::HashMap;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info};

/// Configuration for an MCP server connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// For stdio transport: command to start the server.
    #[serde(default)]
    pub command: Option<String>,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// For HTTP transport: server URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// A connected MCP server with discovered tools.
pub struct McpConnection {
    pub config: McpServerConfig,
    pub tools: Vec<McpToolDef>,
    transport: McpTransport,
    next_id: u64,
}

enum McpTransport {
    Stdio {
        child: Child,
        #[allow(dead_code)]
        stdin_buf: Vec<u8>,
    },
    Http {
        url: String,
        client: reqwest::Client,
    },
}

/// Tool definition from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

/// MCP client manager — manages multiple server connections.
pub struct McpClientManager {
    connections: HashMap<String, McpConnection>,
}

impl McpClientManager {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// Connect to an MCP server and discover its tools.
    pub async fn connect(&mut self, config: McpServerConfig) -> anyhow::Result<Vec<McpToolDef>> {
        let name = config.name.clone();

        let mut conn = if let Some(url) = &config.url {
            // HTTP transport
            McpConnection {
                config: config.clone(),
                tools: Vec::new(),
                transport: McpTransport::Http {
                    url: url.clone(),
                    client: reqwest::Client::new(),
                },
                next_id: 1,
            }
        } else if let Some(command) = &config.command {
            // Stdio transport — spawn the server process
            let mut cmd = Command::new(command);
            cmd.args(&config.args);
            for (k, v) in &config.env {
                cmd.env(k, v);
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());

            let child = cmd.spawn()?;

            McpConnection {
                config: config.clone(),
                tools: Vec::new(),
                transport: McpTransport::Stdio {
                    child,
                    stdin_buf: Vec::new(),
                },
                next_id: 1,
            }
        } else {
            anyhow::bail!("MCP server config must have either 'url' or 'command'");
        };

        // Initialize handshake
        let init_result = conn
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "forgefleet-agent",
                        "version": "0.1.0"
                    }
                }),
            )
            .await?;

        debug!(server = %name, "MCP server initialized: {init_result}");

        // Send initialized notification
        conn.send_notification("notifications/initialized", json!({}))
            .await?;

        // Discover tools
        let tools_result = conn.send_request("tools/list", json!({})).await?;
        let tools: Vec<McpToolDef> =
            if let Some(tools_arr) = tools_result.get("tools").and_then(Value::as_array) {
                tools_arr
                    .iter()
                    .filter_map(|t| serde_json::from_value(t.clone()).ok())
                    .collect()
            } else {
                Vec::new()
            };

        info!(server = %name, tool_count = tools.len(), "MCP tools discovered");

        conn.tools = tools.clone();
        self.connections.insert(name, conn);

        Ok(tools)
    }

    /// Call a tool on a connected MCP server.
    pub async fn call_tool(
        &mut self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<String> {
        let conn = self
            .connections
            .get_mut(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' not connected"))?;

        let result = conn
            .send_request(
                "tools/call",
                json!({
                    "name": tool_name,
                    "arguments": arguments,
                }),
            )
            .await?;

        // Extract text content from result
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            let texts: Vec<&str> = content
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect();
            Ok(texts.join("\n"))
        } else {
            Ok(result.to_string())
        }
    }

    /// List all tools from all connected servers.
    pub fn all_tools(&self) -> Vec<(String, McpToolDef)> {
        self.connections
            .iter()
            .flat_map(|(name, conn)| {
                let name = name.clone();
                conn.tools
                    .iter()
                    .map(move |t| (format!("{}_{}", name, t.name), t.clone()))
            })
            .collect()
    }

    /// List connected servers.
    pub fn list_servers(&self) -> Vec<McpServerInfo> {
        self.connections
            .iter()
            .map(|(name, conn)| McpServerInfo {
                name: name.clone(),
                tool_count: conn.tools.len(),
                transport: if conn.config.url.is_some() {
                    "http"
                } else {
                    "stdio"
                }
                .into(),
            })
            .collect()
    }

    /// Disconnect from a server.
    pub async fn disconnect(&mut self, server_name: &str) {
        if let Some(mut conn) = self.connections.remove(server_name) {
            if let McpTransport::Stdio { ref mut child, .. } = conn.transport {
                let _ = child.kill().await;
            }
            info!(server = %server_name, "MCP server disconnected");
        }
    }

    /// Disconnect all servers.
    pub async fn disconnect_all(&mut self) {
        let names: Vec<String> = self.connections.keys().cloned().collect();
        for name in names {
            self.disconnect(&name).await;
        }
    }
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerInfo {
    pub name: String,
    pub tool_count: usize,
    pub transport: String,
}

impl McpConnection {
    async fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        match &mut self.transport {
            McpTransport::Http { url, client } => {
                let resp = client.post(url.as_str()).json(&request).send().await?;
                let body: Value = resp.json().await?;
                if let Some(result) = body.get("result") {
                    Ok(result.clone())
                } else if let Some(error) = body.get("error") {
                    anyhow::bail!("MCP error: {error}")
                } else {
                    Ok(body)
                }
            }
            McpTransport::Stdio { child, .. } => {
                let stdin = child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
                let mut line = serde_json::to_string(&request)?;
                line.push('\n');
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;

                let stdout = child
                    .stdout
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
                let mut reader = BufReader::new(stdout);
                let mut response_line = String::new();
                reader.read_line(&mut response_line).await?;

                let body: Value = serde_json::from_str(&response_line)?;
                if let Some(result) = body.get("result") {
                    Ok(result.clone())
                } else if let Some(error) = body.get("error") {
                    anyhow::bail!("MCP error: {error}")
                } else {
                    Ok(body)
                }
            }
        }
    }

    async fn send_notification(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        match &mut self.transport {
            McpTransport::Http { url, client } => {
                client.post(url.as_str()).json(&notification).send().await?;
            }
            McpTransport::Stdio { child, .. } => {
                let stdin = child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
                let mut line = serde_json::to_string(&notification)?;
                line.push('\n');
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
        }
        Ok(())
    }
}
