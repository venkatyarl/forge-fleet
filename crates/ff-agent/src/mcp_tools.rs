//! MCP Tool integration — expose MCP server tools as AgentTool implementations.
//!
//! This bridges the MCP client (`mcp_client.rs`) into the agent tool system so
//! the agent loop can discover and invoke tools from external MCP servers
//! (filesystem, GitHub, databases, Slack, etc.) alongside built-in tools.
//!
//! Configuration is read from `~/.forgefleet/mcp.json`:
//! ```json
//! {
//!   "servers": [
//!     { "name": "filesystem", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"] },
//!     { "name": "github", "url": "http://localhost:3100" }
//!   ]
//! }
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::mcp_client::{McpClientManager, McpServerConfig, McpToolDef};
use crate::tools::{AgentTool, AgentToolContext, AgentToolResult};

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Top-level MCP configuration, loaded from `~/.forgefleet/mcp.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    /// List of MCP servers to connect to.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// Discovered MCP tool with its owning server name.
#[derive(Clone)]
pub struct McpAgentTool {
    /// Server name this tool belongs to.
    pub server_name: String,
    /// Tool definition from the MCP server.
    pub tool_def: McpToolDef,
    /// Shared MCP client manager for making calls.
    manager: Arc<Mutex<McpClientManager>>,
}

impl std::fmt::Debug for McpAgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAgentTool")
            .field("server_name", &self.server_name)
            .field("tool_def", &self.tool_def)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// AgentTool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl AgentTool for McpAgentTool {
    fn name(&self) -> &str {
        // Prefix with server name to avoid collisions (e.g. "filesystem_read_file").
        // The name is stored in tool_def.name since we prepend at registration time.
        &self.tool_def.name
    }

    fn description(&self) -> &str {
        &self.tool_def.description
    }

    fn parameters_schema(&self) -> Value {
        self.tool_def.input_schema.clone()
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        // Strip the server prefix from the tool name to get the original MCP tool name.
        let original_name = self.tool_def.name
            .strip_prefix(&format!("{}_", self.server_name))
            .unwrap_or(&self.tool_def.name);

        let mut manager = self.manager.lock().await;
        match manager.call_tool(&self.server_name, original_name, input).await {
            Ok(output) => AgentToolResult::ok(output),
            Err(err) => AgentToolResult::err(format!("MCP tool error: {err}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Default config path: `~/.forgefleet/mcp.json`.
fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("mcp.json")
}

/// Load the MCP configuration from disk, returning an empty config if missing.
pub fn load_mcp_config() -> McpConfig {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(config) => {
                debug!(path = %path.display(), "loaded MCP config");
                config
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "failed to parse MCP config");
                McpConfig::default()
            }
        },
        Err(_) => {
            debug!(path = %path.display(), "no MCP config found — skipping");
            McpConfig::default()
        }
    }
}

/// Discover and connect to all configured MCP servers.
///
/// Returns a list of `McpAgentTool`s that can be added to the agent's tool
/// list, plus the shared `McpClientManager` (for cleanup on session end).
pub async fn discover_mcp_tools() -> (Vec<Arc<dyn AgentTool>>, Arc<Mutex<McpClientManager>>) {
    let config = load_mcp_config();
    let manager = Arc::new(Mutex::new(McpClientManager::new()));
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();

    for server_config in config.servers {
        let server_name = server_config.name.clone();
        let mut mgr = manager.lock().await;

        match mgr.connect(server_config).await {
            Ok(discovered) => {
                info!(
                    server = %server_name,
                    tool_count = discovered.len(),
                    "connected to MCP server"
                );

                for tool_def in discovered {
                    let prefixed_name = format!("{}_{}", server_name, tool_def.name);
                    let prefixed_def = McpToolDef {
                        name: prefixed_name,
                        description: tool_def.description.clone(),
                        input_schema: tool_def.input_schema.clone(),
                    };

                    tools.push(Arc::new(McpAgentTool {
                        server_name: server_name.clone(),
                        tool_def: prefixed_def,
                        manager: manager.clone(),
                    }));
                }
            }
            Err(err) => {
                warn!(server = %server_name, error = %err, "failed to connect to MCP server");
            }
        }
    }

    (tools, manager)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_when_no_file() {
        let config = load_mcp_config();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn parse_mcp_config() {
        let json = r#"{
            "servers": [
                { "name": "fs", "command": "npx", "args": ["-y", "@mcp/fs"] },
                { "name": "api", "url": "http://localhost:3100" }
            ]
        }"#;

        let config: McpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].name, "fs");
        assert!(config.servers[0].command.is_some());
        assert_eq!(config.servers[1].name, "api");
        assert!(config.servers[1].url.is_some());
    }
}
