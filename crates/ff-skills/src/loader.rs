//! Skill loader — parse SKILL.md, MCP JSON tool definitions, and executable
//! tool metadata from various formats.

use std::path::Path;

use chrono::Utc;
use tracing::{debug, info};
use uuid::Uuid;

use crate::error::{Result, SkillError};
use crate::types::{
    SkillMetadata, SkillOrigin, SkillPermission, ToolDefinition, ToolInvocation, ToolParameter,
};

// ─── OpenClaw SKILL.md loader ────────────────────────────────────────────────

/// Load a skill from an OpenClaw-format `SKILL.md` file.
///
/// ## Expected SKILL.md structure
///
/// ```markdown
/// # Skill Name
///
/// Description paragraph(s).
///
/// ## Tools
///
/// ### tool_name
/// Description of the tool.
///
/// **Parameters:**
/// - `param_name` (type, required): description
/// - `other_param` (string): optional param
///
/// **Command:** `shell command here`
/// ```
pub async fn load_openclaw_skill(path: &Path) -> Result<SkillMetadata> {
    if !path.exists() {
        return Err(SkillError::FileNotFound {
            path: path.to_path_buf(),
        });
    }

    let content = tokio::fs::read_to_string(path).await?;
    let skill_dir = path.parent().unwrap_or(Path::new("."));

    // Derive skill id from parent directory name.
    let id = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let (name, description) = parse_skill_header(&content);
    let tools = parse_skill_tools(&content, skill_dir);
    let tags = extract_tags(&content);
    let permissions = infer_permissions(&tools);

    info!(skill = %id, tools = tools.len(), "loaded OpenClaw skill");

    Ok(SkillMetadata {
        id,
        name,
        description,
        origin: SkillOrigin::OpenClaw,
        location: Some(skill_dir.to_path_buf()),
        version: None,
        author: None,
        tags,
        tools,
        permissions,
        registered_at: Utc::now(),
        uuid: Uuid::new_v4(),
        search_keywords: Vec::new(),
    })
}

/// Parse the skill name (first H1) and description (text until next heading).
fn parse_skill_header(content: &str) -> (String, String) {
    let mut name = String::new();
    let mut desc_lines: Vec<&str> = Vec::new();
    let mut in_desc = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if name.is_empty() {
            if let Some(h1) = trimmed.strip_prefix("# ") {
                name = h1.trim().to_string();
                in_desc = true;
                continue;
            }
        } else if in_desc {
            // Stop at the next heading.
            if trimmed.starts_with("## ") {
                break;
            }
            desc_lines.push(trimmed);
        }
    }

    let description = desc_lines
        .into_iter()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    if name.is_empty() {
        name = "Unknown Skill".into();
    }

    (name, description)
}

/// Parse tool definitions from `## Tools` or `### tool_name` sections.
fn parse_skill_tools(content: &str, skill_dir: &Path) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Look for ### headings under a ## Tools section.
        if let Some(tool_name) = trimmed.strip_prefix("### ") {
            let tool_name = tool_name.trim().to_string();
            i += 1;

            // Collect description lines.
            let mut desc_lines = Vec::new();
            let mut params = Vec::new();
            let mut command: Option<String> = None;
            let mut timeout_secs = 30u64;

            while i < lines.len() {
                let line = lines[i].trim();

                // Another heading of same or higher level → done with this tool.
                if line.starts_with("### ") || line.starts_with("## ") || line.starts_with("# ") {
                    break;
                }

                // Parameters section.
                if line.starts_with("- `") || line.starts_with("* `") {
                    if let Some(param) = parse_param_line(line) {
                        params.push(param);
                    }
                }
                // Command extraction.
                else if let Some(cmd) = line
                    .strip_prefix("**Command:**")
                    .or_else(|| line.strip_prefix("**command:**"))
                {
                    let cmd = cmd.trim().trim_matches('`').to_string();
                    if !cmd.is_empty() {
                        command = Some(cmd);
                    }
                }
                // Timeout extraction.
                else if let Some(t) = line
                    .strip_prefix("**Timeout:**")
                    .or_else(|| line.strip_prefix("**timeout:**"))
                {
                    if let Ok(secs) = t.trim().trim_end_matches('s').parse::<u64>() {
                        timeout_secs = secs;
                    }
                }
                // Regular description lines.
                else if !line.is_empty()
                    && !line.starts_with("**Parameters")
                    && !line.starts_with("**parameters")
                {
                    desc_lines.push(line);
                }

                i += 1;
            }

            let description = desc_lines.join(" ").trim().to_string();
            let invocation = match command {
                Some(cmd) => ToolInvocation::Shell {
                    command: cmd,
                    working_dir: Some(skill_dir.to_path_buf()),
                },
                None => ToolInvocation::Prompt {
                    template: description.clone(),
                },
            };

            tools.push(ToolDefinition {
                name: tool_name,
                description,
                parameters: params,
                invocation,
                permissions: Vec::new(),
                timeout_secs,
            });

            continue; // Don't increment i again.
        }

        i += 1;
    }

    tools
}

/// Parse a parameter line like: `- \`name\` (string, required): description`
fn parse_param_line(line: &str) -> Option<ToolParameter> {
    // Strip leading - or * and backticks.
    let line = line.trim_start_matches(|c: char| c == '-' || c == '*' || c.is_whitespace());

    // Extract name between backticks.
    let name_end = line.find('`').and_then(|start| {
        let rest = &line[start + 1..];
        rest.find('`').map(|end| (start + 1, start + 1 + end))
    });

    let (name, rest) = match name_end {
        Some((s, e)) => (line[s..e].to_string(), &line[e + 1..]),
        None => return None,
    };

    // Extract type and required from parentheses.
    let mut param_type = "string".to_string();
    let mut required = false;
    let mut description = rest.to_string();

    if let Some(paren_start) = rest.find('(')
        && let Some(paren_end) = rest[paren_start..].find(')')
    {
        let inside = &rest[paren_start + 1..paren_start + paren_end];
        let parts: Vec<&str> = inside.split(',').map(|s| s.trim()).collect();
        if let Some(t) = parts.first() {
            param_type = t.to_string();
        }
        required = parts.iter().any(|p| p.eq_ignore_ascii_case("required"));
        description = rest[paren_start + paren_end + 1..]
            .trim_start_matches(':')
            .trim()
            .to_string();
    }

    Some(ToolParameter {
        name,
        param_type,
        description,
        required,
        default: None,
        enum_values: Vec::new(),
    })
}

/// Extract tags from markdown content (looks for **Tags:** or frontmatter-style).
fn extract_tags(content: &str) -> Vec<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(tags_str) = trimmed
            .strip_prefix("**Tags:**")
            .or_else(|| trimmed.strip_prefix("tags:"))
        {
            return tags_str
                .split(',')
                .map(|t| {
                    t.trim()
                        .trim_matches(|c: char| c == '`' || c == '"')
                        .to_string()
                })
                .filter(|t| !t.is_empty())
                .collect();
        }
    }
    Vec::new()
}

/// Infer permissions from tool definitions.
fn infer_permissions(tools: &[ToolDefinition]) -> Vec<SkillPermission> {
    let mut perms = Vec::new();
    let mut has_shell = false;
    let mut has_network = false;

    for tool in tools {
        match &tool.invocation {
            ToolInvocation::Shell { .. } => has_shell = true,
            ToolInvocation::Http { .. } => has_network = true,
            _ => {}
        }
        // If any tool has explicit permissions, include them.
        for perm in &tool.permissions {
            if !perms.contains(perm) {
                perms.push(perm.clone());
            }
        }
    }

    if has_shell && !perms.contains(&SkillPermission::ShellExec) {
        perms.push(SkillPermission::ShellExec);
    }
    if has_network && !perms.contains(&SkillPermission::Network) {
        perms.push(SkillPermission::Network);
    }

    perms
}

// ─── MCP JSON loader ─────────────────────────────────────────────────────────

/// MCP tool definition JSON format.
#[derive(Debug, serde::Deserialize)]
struct McpToolJson {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    input_schema: Option<McpInputSchema>,
}

#[derive(Debug, serde::Deserialize)]
struct McpInputSchema {
    #[serde(default)]
    properties: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    required: Vec<String>,
}

/// Load tools from an MCP-format JSON file.
///
/// The file may contain a single tool object or an array of tools, or an
/// object with a `"tools"` key containing an array.
pub async fn load_mcp_tools(path: &Path) -> Result<SkillMetadata> {
    if !path.exists() {
        return Err(SkillError::FileNotFound {
            path: path.to_path_buf(),
        });
    }

    let content = tokio::fs::read_to_string(path).await?;
    let parsed: serde_json::Value = serde_json::from_str(&content)?;

    let mcp_tools: Vec<McpToolJson> = if let Some(arr) = parsed.as_array() {
        serde_json::from_value(serde_json::Value::Array(arr.clone()))?
    } else if let Some(obj) = parsed.as_object() {
        if let Some(tools_val) = obj.get("tools") {
            serde_json::from_value(tools_val.clone())?
        } else {
            vec![serde_json::from_value(parsed.clone())?]
        }
    } else {
        return Err(SkillError::McpParse {
            reason: "expected JSON object or array".into(),
        });
    };

    let skill_dir = path.parent().unwrap_or(Path::new("."));
    let id = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mcp-tools")
        .to_string();

    let tools: Vec<ToolDefinition> = mcp_tools.into_iter().map(convert_mcp_tool).collect();

    info!(skill = %id, tools = tools.len(), "loaded MCP tools");

    Ok(SkillMetadata {
        id: id.clone(),
        name: id,
        description: format!("MCP tools from {}", path.display()),
        origin: SkillOrigin::Mcp,
        location: Some(skill_dir.to_path_buf()),
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

/// Convert an MCP tool definition to our universal ToolDefinition.
fn convert_mcp_tool(mcp: McpToolJson) -> ToolDefinition {
    let parameters: Vec<ToolParameter> = match mcp.input_schema {
        Some(schema) => schema
            .properties
            .into_iter()
            .map(|(name, prop)| {
                let param_type = prop
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("string")
                    .to_string();
                let description = prop
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let required = schema.required.contains(&name);
                let default = prop.get("default").cloned();
                let enum_values = prop
                    .get("enum")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();

                ToolParameter {
                    name,
                    param_type,
                    description,
                    required,
                    default,
                    enum_values,
                }
            })
            .collect(),
        None => Vec::new(),
    };

    ToolDefinition {
        name: mcp.name.clone(),
        description: mcp.description.unwrap_or_default(),
        parameters,
        invocation: ToolInvocation::Http {
            url: String::new(), // Filled in by the MCP adapter at runtime.
            method: "POST".into(),
            headers: std::collections::HashMap::new(),
        },
        permissions: vec![SkillPermission::Network],
        timeout_secs: 30,
    }
}

// ─── Claude tool definition loader ───────────────────────────────────────────

/// Claude tool definition format.
#[derive(Debug, serde::Deserialize)]
struct ClaudeToolJson {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: Option<serde_json::Value>,
}

/// Load Claude-format tool definitions from JSON.
pub async fn load_claude_tools(path: &Path) -> Result<SkillMetadata> {
    if !path.exists() {
        return Err(SkillError::FileNotFound {
            path: path.to_path_buf(),
        });
    }

    let content = tokio::fs::read_to_string(path).await?;
    let tools_json: Vec<ClaudeToolJson> = serde_json::from_str(&content)?;

    let skill_dir = path.parent().unwrap_or(Path::new("."));
    let id = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("claude-tools")
        .to_string();

    let tools: Vec<ToolDefinition> = tools_json
        .into_iter()
        .map(|ct| {
            let schema = ct.input_schema.as_ref();
            let required: Vec<String> = schema
                .and_then(|s| s.get("required"))
                .and_then(|r| serde_json::from_value(r.clone()).ok())
                .unwrap_or_default();

            let params = schema
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object())
                .map(|props| {
                    props
                        .iter()
                        .map(|(name, prop)| ToolParameter {
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
                            required: required.contains(name),
                            default: prop.get("default").cloned(),
                            enum_values: Vec::new(),
                            name: name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();

            ToolDefinition {
                name: ct.name,
                description: ct.description.unwrap_or_default(),
                parameters: params,
                invocation: ToolInvocation::Builtin {
                    handler: "claude_proxy".into(),
                },
                permissions: Vec::new(),
                timeout_secs: 60,
            }
        })
        .collect();

    debug!(skill = %id, tools = tools.len(), "loaded Claude tool definitions");

    Ok(SkillMetadata {
        id: id.clone(),
        name: id,
        description: format!("Claude tools from {}", path.display()),
        origin: SkillOrigin::Claude,
        location: Some(skill_dir.to_path_buf()),
        version: None,
        author: None,
        tags: vec!["claude".into()],
        tools,
        permissions: Vec::new(),
        registered_at: Utc::now(),
        uuid: Uuid::new_v4(),
        search_keywords: Vec::new(),
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_header() {
        let md = "# Weather Skill\n\nGet weather data for any location.\n\n## Tools\n";
        let (name, desc) = parse_skill_header(md);
        assert_eq!(name, "Weather Skill");
        assert!(desc.contains("weather data"));
    }

    #[test]
    fn test_parse_tools() {
        let md = r#"# Test

Desc.

## Tools

### get_weather
Get the current weather.

**Parameters:**
- `location` (string, required): City name
- `units` (string): Temperature units

**Command:** `curl https://api.weather.com/$location`
"#;
        let tools = parse_skill_tools(md, Path::new("/test"));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
        assert_eq!(tools[0].parameters.len(), 2);
        assert!(tools[0].parameters[0].required);
        assert!(!tools[0].parameters[1].required);
    }

    #[test]
    fn test_parse_param_line() {
        let line = "- `city` (string, required): The city name";
        let param = parse_param_line(line).unwrap();
        assert_eq!(param.name, "city");
        assert_eq!(param.param_type, "string");
        assert!(param.required);
        assert!(param.description.contains("city name"));
    }

    #[test]
    fn test_extract_tags() {
        let md = "# Skill\n\n**Tags:** weather, forecast, api\n";
        let tags = extract_tags(md);
        assert_eq!(tags, vec!["weather", "forecast", "api"]);
    }

    #[test]
    fn test_mcp_tool_conversion() {
        let mcp = McpToolJson {
            name: "search".into(),
            description: Some("Search the web".into()),
            input_schema: Some(McpInputSchema {
                properties: {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "query".into(),
                        serde_json::json!({"type": "string", "description": "Search query"}),
                    );
                    m
                },
                required: vec!["query".into()],
            }),
        };
        let tool = convert_mcp_tool(mcp);
        assert_eq!(tool.name, "search");
        assert_eq!(tool.parameters.len(), 1);
        assert!(tool.parameters[0].required);
    }
}
