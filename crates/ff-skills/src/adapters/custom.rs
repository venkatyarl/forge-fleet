//! Custom / user-defined skill adapter.
//!
//! Handles free-form skill definitions that don't match OpenClaw, Claude, or MCP
//! formats.  Provides a flexible import path for arbitrary tool registrations.

use chrono::Utc;
use uuid::Uuid;

use crate::adapters::SkillAdapter;
use crate::error::{Result, SkillError};
use crate::types::{
    SkillMetadata, SkillOrigin, SkillPermission, ToolDefinition, ToolInvocation, ToolParameter,
};

/// Adapter for custom / user-defined skill formats.
///
/// This is the fallback adapter — it accepts a minimal JSON structure and
/// creates a skill from it.  Useful for programmatic tool registration.
#[derive(Debug, Default)]
pub struct CustomAdapter;

impl CustomAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl SkillAdapter for CustomAdapter {
    fn name(&self) -> &str {
        "custom"
    }

    /// Import from a generic JSON format.
    ///
    /// Minimal required fields: `name`.  Everything else is optional.
    ///
    /// ```json
    /// {
    ///   "name": "my-tool",
    ///   "description": "Does something",
    ///   "tools": [{
    ///     "name": "run",
    ///     "description": "Execute the thing",
    ///     "type": "shell",
    ///     "command": "echo hello",
    ///     "timeout": 30
    ///   }]
    /// }
    /// ```
    fn import(&self, raw: &serde_json::Value) -> Result<SkillMetadata> {
        let obj = raw.as_object().ok_or_else(|| SkillError::InvalidManifest {
            reason: "expected JSON object for custom skill".into(),
        })?;

        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SkillError::InvalidManifest {
                reason: "custom skill must have 'name' field".into(),
            })?
            .to_string();

        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Custom skill")
            .to_string();

        let tags: Vec<String> = obj
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let tools: Vec<ToolDefinition> = obj
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(parse_custom_tool).collect())
            .unwrap_or_default();

        let permissions: Vec<SkillPermission> = obj
            .get("permissions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(parse_permission)
                    .collect()
            })
            .unwrap_or_default();

        Ok(SkillMetadata {
            id: name.clone(),
            name,
            description,
            origin: SkillOrigin::Custom,
            location: obj
                .get("location")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from),
            version: obj
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from),
            author: obj.get("author").and_then(|v| v.as_str()).map(String::from),
            tags,
            tools,
            permissions,
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        })
    }

    /// Export to a generic JSON format.
    fn export(&self, skill: &SkillMetadata) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(skill)?)
    }

    /// Custom adapter is the fallback — it can handle anything with a "name".
    fn can_handle(&self, raw: &serde_json::Value) -> bool {
        raw.get("name").and_then(|v| v.as_str()).is_some()
    }
}

/// Parse a tool from custom JSON format.
fn parse_custom_tool(val: &serde_json::Value) -> Option<ToolDefinition> {
    let obj = val.as_object()?;
    let name = obj.get("name")?.as_str()?.to_string();
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tool_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("prompt");

    let invocation = match tool_type {
        "shell" | "command" => {
            let command = obj
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("echo 'no command specified'")
                .to_string();
            ToolInvocation::Shell {
                command,
                working_dir: obj
                    .get("working_dir")
                    .and_then(|v| v.as_str())
                    .map(std::path::PathBuf::from),
            }
        }
        "http" | "api" => {
            let url = obj
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let method = obj
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("POST")
                .to_string();
            ToolInvocation::Http {
                url,
                method,
                headers: std::collections::HashMap::new(),
            }
        }
        "builtin" => {
            let handler = obj
                .get("handler")
                .and_then(|v| v.as_str())
                .unwrap_or("noop")
                .to_string();
            ToolInvocation::Builtin { handler }
        }
        _ => ToolInvocation::Prompt {
            template: obj
                .get("template")
                .and_then(|v| v.as_str())
                .unwrap_or(&description)
                .to_string(),
        },
    };

    let parameters: Vec<ToolParameter> = obj
        .get("parameters")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let po = p.as_object()?;
                    Some(ToolParameter {
                        name: po.get("name")?.as_str()?.to_string(),
                        param_type: po
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("string")
                            .to_string(),
                        description: po
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        required: po
                            .get("required")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        default: po.get("default").cloned(),
                        enum_values: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let timeout_secs = obj.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);

    Some(ToolDefinition {
        name,
        description,
        parameters,
        invocation,
        permissions: Vec::new(),
        timeout_secs,
    })
}

/// Parse a permission string into a SkillPermission.
fn parse_permission(s: &str) -> Option<SkillPermission> {
    match s {
        "file:read" | "file_read" => Some(SkillPermission::FileRead),
        "file:write" | "file_write" => Some(SkillPermission::FileWrite),
        "shell:exec" | "shell_exec" => Some(SkillPermission::ShellExec),
        "network" => Some(SkillPermission::Network),
        "env:access" | "env_access" => Some(SkillPermission::EnvAccess),
        "secrets" => Some(SkillPermission::Secrets),
        "process:spawn" | "process_spawn" => Some(SkillPermission::ProcessSpawn),
        other => Some(SkillPermission::Custom(other.to_string())),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_import_custom() {
        let raw = serde_json::json!({
            "name": "my-tool",
            "description": "Custom tool",
            "tools": [{
                "name": "run",
                "description": "Run it",
                "type": "shell",
                "command": "echo hello"
            }],
            "permissions": ["shell:exec"]
        });

        let adapter = CustomAdapter::new();
        let skill = adapter.import(&raw).unwrap();
        assert_eq!(skill.name, "my-tool");
        assert_eq!(skill.tools.len(), 1);
        assert_eq!(skill.origin, SkillOrigin::Custom);
        assert!(skill.permissions.contains(&SkillPermission::ShellExec));
    }

    #[test]
    fn test_export_custom() {
        let adapter = CustomAdapter::new();
        let skill = SkillMetadata {
            id: "t".into(),
            name: "t".into(),
            description: "Test".into(),
            origin: SkillOrigin::Custom,
            location: None,
            version: None,
            author: None,
            tags: Vec::new(),
            tools: Vec::new(),
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        };
        let val = adapter.export(&skill).unwrap();
        assert_eq!(val["name"], "t");
    }

    #[test]
    fn test_parse_permission() {
        assert_eq!(
            parse_permission("shell:exec"),
            Some(SkillPermission::ShellExec)
        );
        assert_eq!(parse_permission("network"), Some(SkillPermission::Network));
        assert_eq!(
            parse_permission("my_custom"),
            Some(SkillPermission::Custom("my_custom".into()))
        );
    }
}
