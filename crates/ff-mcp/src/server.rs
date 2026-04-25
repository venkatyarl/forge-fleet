//! MCP Server — routes JSON-RPC requests to handlers.
//!
//! The `McpServer` holds the tool registry and processes incoming
//! JSON-RPC 2.0 requests, dispatching them to the appropriate handler
//! and wrapping results in proper JSON-RPC responses.

use std::collections::HashSet;

use ff_core::config;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::federation;
use crate::handlers;
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::tools::ToolRegistry;

// ─── MCP Server ──────────────────────────────────────────────────────────────

/// The core MCP server. Holds the tool registry and dispatches requests.
#[derive(Debug, Clone)]
pub struct McpServer {
    registry: ToolRegistry,
}

impl McpServer {
    /// Create a new MCP server with the default tool registry.
    pub fn new() -> Self {
        Self {
            registry: ToolRegistry::new(),
        }
    }

    /// Create a server with a custom tool registry.
    pub fn with_registry(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    /// Get a reference to the tool registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Process a single JSON-RPC request and return a response.
    ///
    /// Handles MCP-specific methods (`initialize`, `tools/list`, `tools/call`)
    /// as well as direct tool invocations by name.
    pub async fn handle_request(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        // Validate JSON-RPC version
        if request.jsonrpc != "2.0" {
            return Some(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_request("jsonrpc must be \"2.0\""),
            ));
        }

        debug!(method = %request.method, "handling MCP request");

        let response = match request.method.as_str() {
            // ── MCP lifecycle ────────────────────────────────────────
            "initialize" => self.handle_initialize(request.id.clone()).await,
            "initialized" => {
                // Notification — no response needed
                return None;
            }

            // ── MCP tool discovery ───────────────────────────────────
            "tools/list" => self.handle_tools_list(request.id.clone()).await,

            // ── MCP tool invocation ──────────────────────────────────
            "tools/call" => {
                self.handle_tools_call(request.id.clone(), request.params)
                    .await
            }

            // ── Direct tool invocation (convenience) ─────────────────
            method => {
                self.handle_direct_call(method, request.id.clone(), request.params)
                    .await
            }
        };

        Some(response)
    }

    // ── MCP Initialize ───────────────────────────────────────────────────

    async fn handle_initialize(&self, id: Option<Value>) -> JsonRpcResponse {
        JsonRpcResponse::success(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "forgefleet-mcp",
                    "version": crate::VERSION
                }
            }),
        )
    }

    // ── Tools List ───────────────────────────────────────────────────────

    async fn handle_tools_list(&self, id: Option<Value>) -> JsonRpcResponse {
        let mut names = HashSet::new();
        let mut tools: Vec<Value> = self
            .registry
            .list()
            .iter()
            .map(|t| {
                names.insert(t.name.clone());
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema
                })
            })
            .collect();

        // Best-effort federation append. Failures should never break local tools/list.
        if let Ok((cfg, _path)) = config::load_config_auto() {
            let federated_tools = federation::list_federated_tools(&cfg, 5).await;
            for tool in federated_tools {
                if names.contains(&tool.name) {
                    continue;
                }

                names.insert(tool.name.clone());
                tools.push(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                    "x-federated": true,
                    "x-source-service": tool.service,
                    "x-source-endpoint": tool.endpoint
                }));
            }
        }

        JsonRpcResponse::success(id, json!({ "tools": tools }))
    }

    // ── Tools Call (MCP standard) ────────────────────────────────────────

    async fn handle_tools_call(&self, id: Option<Value>, params: Option<Value>) -> JsonRpcResponse {
        // Extract tool name from params
        let tool_name = params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let Some(tool_name) = tool_name else {
            return JsonRpcResponse::error(
                id,
                JsonRpcError::invalid_params("missing 'name' in tools/call params"),
            );
        };

        // Extract the arguments for the tool
        let arguments = params.and_then(|p| p.get("arguments").cloned());

        let result = if self.registry.contains(&tool_name) {
            handlers::dispatch(&tool_name, arguments.clone()).await
        } else {
            self.try_federated_tool_call(&tool_name, arguments.clone())
                .await
        };

        match result {
            Ok(result) => JsonRpcResponse::success(
                id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&result).unwrap_or_default()
                    }]
                }),
            ),
            Err(e) => {
                if e.contains("not found") {
                    JsonRpcResponse::error(id, JsonRpcError::method_not_found(&tool_name))
                } else {
                    JsonRpcResponse::error(id, JsonRpcError::internal_error(e))
                }
            }
        }
    }

    // ── Direct tool call (convenience, non-MCP) ──────────────────────────

    async fn handle_direct_call(
        &self,
        method: &str,
        id: Option<Value>,
        params: Option<Value>,
    ) -> JsonRpcResponse {
        let known_locally = self.registry.contains(method);
        let result = if known_locally {
            handlers::dispatch(method, params.clone()).await
        } else {
            self.try_federated_tool_call(method, params.clone()).await
        };

        match result {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(e) => {
                warn!(method, error = %e, "MCP method failed");
                // Methods not in our registry that also can't be served via
                // federation are METHOD_NOT_FOUND regardless of *why*
                // federation declined (no targets, target unreachable,
                // target doesn't expose this name). INTERNAL_ERROR is
                // reserved for genuine handler failures.
                if !known_locally {
                    JsonRpcResponse::error(id, JsonRpcError::method_not_found(method))
                } else {
                    JsonRpcResponse::error(id, JsonRpcError::internal_error(e))
                }
            }
        }
    }

    async fn try_federated_tool_call(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, String> {
        let Ok((cfg, _path)) = config::load_config_auto() else {
            return Err(format!("federated tool '{tool_name}' not found"));
        };

        federation::call_federated_tool(&cfg, tool_name, arguments, 5).await
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_request(method: &str, params: Option<Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: Some(Value::Number(1.into())),
        }
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let server = McpServer::new();
        let req = make_request("initialize", None);
        let resp = server.handle_request(req).await.unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "forgefleet-mcp");
    }

    #[tokio::test]
    async fn tools_list_returns_all_tools() {
        let server = McpServer::new();
        let req = make_request("tools/list", None);
        let resp = server.handle_request(req).await.unwrap();
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert!(tools.len() >= 11);
    }

    #[tokio::test]
    async fn tools_call_dispatches_correctly() {
        let server = McpServer::new();
        let req = make_request(
            "tools/call",
            Some(json!({ "name": "fleet_status", "arguments": { "refresh": false } })),
        );
        let resp = server.handle_request(req).await.unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn direct_call_works() {
        let server = McpServer::new();
        let req = make_request("fleet_status", Some(json!({ "refresh": false })));
        let resp = server.handle_request(req).await.unwrap();
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let server = McpServer::new();
        let req = make_request("nonexistent_method", None);
        let resp = server.handle_request(req).await.unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn initialized_notification_returns_none() {
        let server = McpServer::new();
        let req = make_request("initialized", None);
        let resp = server.handle_request(req).await;
        assert!(resp.is_none());
    }
}
