//! Claude / Anthropic tool adapter.
//!
//! Converts between Claude's tool-use format and ForgeFleet's universal model.
//! Claude tools use JSON Schema for `input_schema` and have a flat structure.

use chrono::Utc;
use uuid::Uuid;

use crate::adapters::SkillAdapter;
use crate::error::{Result, SkillError};
use crate::types::{SkillMetadata, SkillOrigin, ToolDefinition, ToolInvocation, ToolParameter};

/// Adapter for Claude / Anthropic tool definitions.
#[derive(Debug, Default)]
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl SkillAdapter for ClaudeAdapter {
    fn name(&self) -> &str {
        "claude"
    }

    /// Import from Claude's tool format.
    ///
    /// Expected: an object with `"tools"` array, or a single tool object, e.g.:
    /// ```json
    /// {
    ///   "name": "get_weather",
    ///   "description": "Get weather for a location",
    ///   "input_schema": {
    ///     "type": "object",
    ///     "properties": { "location": { "type": "string" } },
    ///     "required": ["location"]
    ///   }
    /// }
    /// ```
    fn import(&self, raw: &serde_json::Value) -> Result<SkillMetadata> {
        let tools_raw: Vec<&serde_json::Value> = if let Some(arr) = raw.as_array() {
            arr.iter().collect()
        } else if raw.get("tools").is_some() {
            raw["tools"]
                .as_array()
                .map(|a| a.iter().collect())
                .unwrap_or_default()
        } else if raw.get("name").is_some() && raw.get("input_schema").is_some() {
            vec![raw]
        } else {
            return Err(SkillError::AdapterError {
                adapter: "claude".into(),
                reason: "unrecognized Claude tool format".into(),
            });
        };

        let tools: Vec<ToolDefinition> = tools_raw
            .into_iter()
            .filter_map(parse_claude_tool)
            .collect();

        let name = tools
            .first()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "claude-tools".into());

        Ok(SkillMetadata {
            id: name.clone(),
            name,
            description: "Imported Claude tool definitions".into(),
            origin: SkillOrigin::Claude,
            location: None,
            version: None,
            author: None,
            tags: vec!["claude".into(), "anthropic".into()],
            tools,
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        })
    }

    /// Export to Claude's tool-use format.
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
                    properties.insert(param.name.clone(), prop);
                    if param.required {
                        required.push(serde_json::Value::String(param.name.clone()));
                    }
                }

                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": {
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
        // Claude tools have "input_schema" (not "inputSchema" like MCP).
        if raw.get("input_schema").is_some() {
            return true;
        }
        if let Some(tools) = raw.get("tools").and_then(|t| t.as_array()) {
            return tools.iter().any(|t| t.get("input_schema").is_some());
        }
        if let Some(arr) = raw.as_array() {
            return arr.iter().any(|t| t.get("input_schema").is_some());
        }
        false
    }
}

/// Parse a single Claude tool definition.
fn parse_claude_tool(val: &serde_json::Value) -> Option<ToolDefinition> {
    let name = val.get("name")?.as_str()?.to_string();
    let description = val
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let schema = val.get("input_schema");
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

    Some(ToolDefinition {
        name,
        description,
        parameters,
        invocation: ToolInvocation::Builtin {
            handler: "claude_proxy".into(),
        },
        permissions: Vec::new(),
        timeout_secs: 60,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_import_claude_single() {
        let raw = serde_json::json!({
            "name": "get_weather",
            "description": "Get current weather",
            "input_schema": {
                "type": "object",
                "properties": {
                    "location": { "type": "string", "description": "City" }
                },
                "required": ["location"]
            }
        });

        let adapter = ClaudeAdapter::new();
        let skill = adapter.import(&raw).unwrap();
        assert_eq!(skill.tools.len(), 1);
        assert_eq!(skill.tools[0].name, "get_weather");
        assert_eq!(skill.tools[0].parameters.len(), 1);
        assert!(skill.tools[0].parameters[0].required);
    }

    #[test]
    fn test_export_claude() {
        let adapter = ClaudeAdapter::new();
        let skill = SkillMetadata {
            id: "test".into(),
            name: "test".into(),
            description: "Test".into(),
            origin: SkillOrigin::Claude,
            location: None,
            version: None,
            author: None,
            tags: Vec::new(),
            tools: vec![ToolDefinition {
                name: "greet".into(),
                description: "Say hello".into(),
                parameters: vec![ToolParameter {
                    name: "name".into(),
                    param_type: "string".into(),
                    description: "Person name".into(),
                    required: true,
                    default: None,
                    enum_values: Vec::new(),
                }],
                invocation: ToolInvocation::Builtin {
                    handler: "test".into(),
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
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "greet");
        assert!(tools[0]["input_schema"]["properties"]["name"].is_object());
    }

    #[test]
    fn test_can_handle_claude() {
        let adapter = ClaudeAdapter::new();
        let yes = serde_json::json!({"name": "x", "input_schema": {}});
        assert!(adapter.can_handle(&yes));

        let no = serde_json::json!({"name": "x", "inputSchema": {}});
        assert!(!adapter.can_handle(&no));
    }
}
