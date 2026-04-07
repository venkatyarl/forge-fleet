//! OpenClaw skill adapter.
//!
//! Converts between OpenClaw's SKILL.md-based format and ForgeFleet's universal
//! skill model.  OpenClaw skills are directory-based: each skill is a folder
//! containing `SKILL.md` plus optional scripts and configs.

use chrono::Utc;
use uuid::Uuid;

use crate::adapters::SkillAdapter;
use crate::error::{Result, SkillError};
use crate::types::{SkillMetadata, SkillOrigin, ToolDefinition, ToolInvocation, ToolParameter};

/// Adapter for OpenClaw-format skills.
#[derive(Debug, Default)]
pub struct OpenClawAdapter;

impl OpenClawAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl SkillAdapter for OpenClawAdapter {
    fn name(&self) -> &str {
        "openclaw"
    }

    /// Import an OpenClaw skill from a JSON representation.
    ///
    /// Expected format:
    /// ```json
    /// {
    ///   "name": "skill-name",
    ///   "description": "...",
    ///   "location": "/path/to/skill",
    ///   "tools": [{ "name": "...", "description": "...", "command": "..." }]
    /// }
    /// ```
    fn import(&self, raw: &serde_json::Value) -> Result<SkillMetadata> {
        let obj = raw.as_object().ok_or_else(|| SkillError::InvalidManifest {
            reason: "expected JSON object".into(),
        })?;

        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let location = obj
            .get("location")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);

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
            .map(|arr| arr.iter().filter_map(parse_openclaw_tool).collect())
            .unwrap_or_default();

        Ok(SkillMetadata {
            id: name.clone(),
            name,
            description,
            origin: SkillOrigin::OpenClaw,
            location,
            version: obj
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from),
            author: obj.get("author").and_then(|v| v.as_str()).map(String::from),
            tags,
            tools,
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        })
    }

    /// Export to OpenClaw JSON format.
    fn export(&self, skill: &SkillMetadata) -> Result<serde_json::Value> {
        let tools: Vec<serde_json::Value> = skill
            .tools
            .iter()
            .map(|t| {
                let mut obj = serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                });
                if let ToolInvocation::Shell { ref command, .. } = t.invocation {
                    obj["command"] = serde_json::Value::String(command.clone());
                }
                obj
            })
            .collect();

        Ok(serde_json::json!({
            "name": skill.name,
            "description": skill.description,
            "location": skill.location,
            "tags": skill.tags,
            "tools": tools,
            "version": skill.version,
            "author": skill.author,
        }))
    }

    fn can_handle(&self, raw: &serde_json::Value) -> bool {
        // OpenClaw skills have a "name" and typically a "location" or "tools"
        // array with "command" entries.
        raw.get("name").is_some()
            && (raw.get("location").is_some()
                || raw
                    .get("tools")
                    .and_then(|t| t.as_array())
                    .map(|arr| arr.iter().any(|t| t.get("command").is_some()))
                    .unwrap_or(false))
    }
}

/// Parse a single tool from OpenClaw JSON format.
fn parse_openclaw_tool(val: &serde_json::Value) -> Option<ToolDefinition> {
    let obj = val.as_object()?;
    let name = obj.get("name")?.as_str()?.to_string();
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let invocation = if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
        ToolInvocation::Shell {
            command: cmd.to_string(),
            working_dir: None,
        }
    } else {
        ToolInvocation::Prompt {
            template: description.clone(),
        }
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_import_openclaw() {
        let raw = serde_json::json!({
            "name": "weather",
            "description": "Get weather data",
            "location": "/skills/weather",
            "tags": ["weather", "api"],
            "tools": [{
                "name": "get_forecast",
                "description": "Get weather forecast",
                "command": "curl https://api.weather.com"
            }]
        });

        let adapter = OpenClawAdapter::new();
        let skill = adapter.import(&raw).unwrap();
        assert_eq!(skill.name, "weather");
        assert_eq!(skill.tools.len(), 1);
        assert_eq!(skill.origin, SkillOrigin::OpenClaw);
    }

    #[test]
    fn test_export_openclaw() {
        let skill = SkillMetadata {
            id: "test".into(),
            name: "test".into(),
            description: "Test skill".into(),
            origin: SkillOrigin::OpenClaw,
            location: Some("/test".into()),
            version: None,
            author: None,
            tags: vec!["test".into()],
            tools: vec![ToolDefinition {
                name: "run".into(),
                description: "Run something".into(),
                parameters: Vec::new(),
                invocation: ToolInvocation::Shell {
                    command: "echo hello".into(),
                    working_dir: None,
                },
                permissions: Vec::new(),
                timeout_secs: 30,
            }],
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        };

        let adapter = OpenClawAdapter::new();
        let exported = adapter.export(&skill).unwrap();
        assert_eq!(exported["name"], "test");
        assert!(exported["tools"][0]["command"].is_string());
    }

    #[test]
    fn test_can_handle() {
        let adapter = OpenClawAdapter::new();
        let valid = serde_json::json!({"name": "x", "location": "/y"});
        assert!(adapter.can_handle(&valid));

        let invalid = serde_json::json!({"tools": [{"inputSchema": {}}]});
        assert!(!adapter.can_handle(&invalid));
    }
}
