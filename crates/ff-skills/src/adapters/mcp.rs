//! Model Context Protocol (MCP) adapter.
//!
//! Converts between MCP's JSON-RPC tool format and ForgeFleet's universal model.
//! MCP tools use `inputSchema` (camelCase) and communicate via JSON-RPC.

use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use crate::adapters::SkillAdapter;
use crate::error::{Result, SkillError};
use crate::types::{
    SkillMetadata, SkillOrigin, SkillPermission, ToolDefinition, ToolInvocation, ToolParameter,
};

/// Adapter for MCP (Model Context Protocol) tool definitions.
#[derive(Debug, Default)]
pub struct McpAdapter {
    /// Base URL of the MCP server (used when exporting).
    pub server_url: Option<String>,
}

impl McpAdapter {
    pub fn new() -> Self {
        Self { server_url: None }
    }

    pub fn with_server_url(url: impl Into<String>) -> Self {
        Self {
            server_url: Some(url.into()),
        }
    }
}

impl SkillAdapter for McpAdapter {
    fn name(&self) -> &str {
        "mcp"
    }

    /// Import from MCP format.
    ///
    /// Accepts:
    /// - A single tool object with `name` + `inputSchema`
    /// - An array of tool objects
    /// - An object with `"tools"` key containing an array
    /// - A JSON-RPC response with `result.tools`
    fn import(&self, raw: &serde_json::Value) -> Result<SkillMetadata> {
        let tools_raw: Vec<serde_json::Value> = extract_mcp_tools(raw)?;

        let tools: Vec<ToolDefinition> = tools_raw
            .iter()
            .filter_map(|t| parse_mcp_tool(t, &self.server_url))
            .collect();

        let id = if tools.len() == 1 {
            tools[0].name.clone()
        } else {
            "mcp-server".into()
        };

        Ok(SkillMetadata {
            id: id.clone(),
            name: id,
            description: "Imported MCP tool definitions".into(),
            origin: SkillOrigin::Mcp,
            location: None,
            version: None,
            author: None,
            tags: vec!["mcp".into()],
            tools,
            permissions: vec![SkillPermission::Network],
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        })
    }

    /// Export to MCP's `tools/list` response format.
    fn export(&self, skill: &SkillMetadata) -> Result<serde_json::Value> {
        let tools: Vec<serde_json::Value> = skill
            .tools
            .iter()
            .map(|t| {
                let mut properties = serde_json::Map::new();
                let mut required = Vec::new();

                for param in &t.parameters {
                    let mut prop = serde_json::json!({
                        "type": param.param_type,
                        "description": param.description,
                    });
                    if let Some(ref default) = param.default {
                        prop["default"] = default.clone();
                    }
                    if !param.enum_values.is_empty() {
                        prop["enum"] = serde_json::json!(param.enum_values);
                    }
                    properties.insert(param.name.clone(), prop);
                    if param.required {
                        required.push(serde_json::Value::String(param.name.clone()));
                    }
                }

                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": {
                        "type": "object",
                        "properties": properties,
                        "required": required,
                    }
                })
            })
            .collect();

        Ok(serde_json::json!({ "tools": tools }))
    }

    fn can_handle(&self, raw: &serde_json::Value) -> bool {
        // MCP uses "inputSchema" (camelCase).
        if raw.get("inputSchema").is_some() {
            return true;
        }
        if let Some(tools) = raw
            .get("tools")
            .or_else(|| raw.pointer("/result/tools"))
            .and_then(|t| t.as_array())
        {
            return tools.iter().any(|t| t.get("inputSchema").is_some());
        }
        if let Some(arr) = raw.as_array() {
            return arr.iter().any(|t| t.get("inputSchema").is_some());
        }
        false
    }
}

/// Extract the tools array from various MCP envelope formats.
fn extract_mcp_tools(raw: &serde_json::Value) -> Result<Vec<serde_json::Value>> {
    // Direct array of tools.
    if let Some(arr) = raw.as_array() {
        return Ok(arr.clone());
    }

    // { "tools": [...] }
    if let Some(tools) = raw.get("tools").and_then(|t| t.as_array()) {
        return Ok(tools.clone());
    }

    // JSON-RPC response: { "result": { "tools": [...] } }
    if let Some(tools) = raw.pointer("/result/tools").and_then(|t| t.as_array()) {
        return Ok(tools.clone());
    }

    // Single tool object.
    if raw.get("name").is_some() && raw.get("inputSchema").is_some() {
        return Ok(vec![raw.clone()]);
    }

    Err(SkillError::McpParse {
        reason: "could not extract tools from MCP payload".into(),
    })
}

/// Parse a single MCP tool definition.
fn parse_mcp_tool(val: &serde_json::Value, server_url: &Option<String>) -> Option<ToolDefinition> {
    let name = val.get("name")?.as_str()?.to_string();
    let description = val
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let schema = val.get("inputSchema");
    let required_list: Vec<String> = schema
        .and_then(|s| s.get("required"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let parameters: Vec<ToolParameter> = schema
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
        .map(|props| {
            props
                .iter()
                .map(|(pname, prop)| ToolParameter {
                    name: pname.clone(),
                    param_type: prop
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("string")
                        .into(),
                    description: prop
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .into(),
                    required: required_list.contains(pname),
                    default: prop.get("default").cloned(),
                    enum_values: prop
                        .get("enum")
                        .and_then(|e| e.as_array())
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    let url = server_url.clone().unwrap_or_default();

    Some(ToolDefinition {
        name,
        description,
        parameters,
        invocation: ToolInvocation::Http {
            url,
            method: "POST".into(),
            headers: HashMap::new(),
        },
        permissions: vec![SkillPermission::Network],
        timeout_secs: 30,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_import_mcp_array() {
        let raw = serde_json::json!([
            {
                "name": "search",
                "description": "Search the web",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" }
                    },
                    "required": ["query"]
                }
            }
        ]);

        let adapter = McpAdapter::new();
        let skill = adapter.import(&raw).unwrap();
        assert_eq!(skill.tools.len(), 1);
        assert_eq!(skill.tools[0].name, "search");
        assert_eq!(skill.origin, SkillOrigin::Mcp);
    }

    #[test]
    fn test_import_mcp_jsonrpc() {
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "read_file",
                    "description": "Read a file",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        },
                        "required": ["path"]
                    }
                }]
            }
        });

        let adapter = McpAdapter::new();
        let skill = adapter.import(&raw).unwrap();
        assert_eq!(skill.tools.len(), 1);
        assert_eq!(skill.tools[0].name, "read_file");
    }

    #[test]
    fn test_export_mcp() {
        let adapter = McpAdapter::new();
        let skill = SkillMetadata {
            id: "test".into(),
            name: "test".into(),
            description: "Test".into(),
            origin: SkillOrigin::Mcp,
            location: None,
            version: None,
            author: None,
            tags: Vec::new(),
            tools: vec![ToolDefinition {
                name: "fetch".into(),
                description: "Fetch URL".into(),
                parameters: vec![ToolParameter {
                    name: "url".into(),
                    param_type: "string".into(),
                    description: "URL to fetch".into(),
                    required: true,
                    default: None,
                    enum_values: Vec::new(),
                }],
                invocation: ToolInvocation::Http {
                    url: "http://localhost:3000".into(),
                    method: "POST".into(),
                    headers: HashMap::new(),
                },
                permissions: Vec::new(),
                timeout_secs: 30,
            }],
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        };

        let exported = adapter.export(&skill).unwrap();
        let tools = exported["tools"].as_array().unwrap();
        assert_eq!(tools[0]["inputSchema"]["required"][0], "url");
    }

    #[test]
    fn test_can_handle_mcp() {
        let adapter = McpAdapter::new();

        // Has inputSchema → MCP
        assert!(adapter.can_handle(&serde_json::json!({"name": "x", "inputSchema": {}})));
        // Has input_schema → NOT MCP (that's Claude)
        assert!(!adapter.can_handle(&serde_json::json!({"name": "x", "input_schema": {}})));
    }
}
