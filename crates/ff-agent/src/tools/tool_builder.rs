//! ToolBuilder — generate, compile, and register new Rust tools at runtime.
//!
//! Unlike SkillBuilder (which creates SKILL.md shell scripts), ToolBuilder
//! creates actual compiled Rust tools that become part of ForgeFleet.
//! The agent writes the Rust code, compiles it, and it's available immediately
//! after a rebuild.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct ToolBuilderTool;

#[async_trait]
impl AgentTool for ToolBuilderTool {
    fn name(&self) -> &str {
        "ToolBuilder"
    }
    fn description(&self) -> &str {
        "Create new compiled Rust tools for ForgeFleet. Generates the tool source code, adds it to the tools module, and optionally triggers a rebuild. The new tool becomes a permanent part of ForgeFleet."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["create","preview","list_custom","delete"],"description":"What to do"},
            "name":{"type":"string","description":"Tool name in PascalCase (e.g. 'MyCustomTool')"},
            "tool_name":{"type":"string","description":"Name as it appears to the LLM (e.g. 'MyCustom')"},
            "description":{"type":"string","description":"What the tool does"},
            "parameters":{"type":"array","items":{"type":"object","properties":{
                "name":{"type":"string"},
                "param_type":{"type":"string","enum":["string","number","boolean","array","object"]},
                "description":{"type":"string"},
                "required":{"type":"boolean"}
            }}},
            "implementation":{"type":"string","description":"Rust code for the execute body (has access to 'input: Value' and 'ctx: &AgentToolContext')"},
            "rebuild":{"type":"boolean","description":"Auto-rebuild ForgeFleet after creating (default: false)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");

        // Find the ForgeFleet source directory
        let ff_root = find_forgefleet_root(ctx).await;

        match action {
            "create" => {
                let struct_name = input.get("name").and_then(Value::as_str).unwrap_or("");
                let tool_name = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
                let description = input
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let implementation = input
                    .get("implementation")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                if struct_name.is_empty() || tool_name.is_empty() || description.is_empty() {
                    return AgentToolResult::err(
                        "'name', 'tool_name', and 'description' are required",
                    );
                }

                // Build parameter schema
                let params = input
                    .get("parameters")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut schema_props = String::new();
                let mut required_list = Vec::new();
                for param in &params {
                    let pname = param.get("name").and_then(Value::as_str).unwrap_or("param");
                    let ptype = param
                        .get("param_type")
                        .and_then(Value::as_str)
                        .unwrap_or("string");
                    let pdesc = param
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let req = param
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);

                    schema_props.push_str(&format!(
                        "                \"{pname}\": {{ \"type\": \"{ptype}\", \"description\": \"{pdesc}\" }},\n"
                    ));
                    if req {
                        required_list.push(format!("\"{pname}\""));
                    }
                }

                // Build the implementation body
                let impl_body = if implementation.is_empty() {
                    "        // TODO: Implement tool logic here\n        AgentToolResult::ok(\"Tool executed successfully\".to_string())".to_string()
                } else {
                    format!("        {implementation}")
                };

                // Generate Rust source
                let file_name = to_snake_case(struct_name);
                let source = format!(
                    r#"//! {description}
//! Auto-generated by ToolBuilder.

use async_trait::async_trait;
use serde_json::{{Value, json}};

use super::{{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output}};

pub struct {struct_name};

#[async_trait]
impl AgentTool for {struct_name} {{
    fn name(&self) -> &str {{ "{tool_name}" }}
    fn description(&self) -> &str {{ "{description}" }}
    fn parameters_schema(&self) -> Value {{
        json!({{
            "type": "object",
            "properties": {{
{schema_props}            }},
            "required": [{required}]
        }})
    }}
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {{
{impl_body}
    }}
}}
"#,
                    required = required_list.join(", ")
                );

                // Save to custom tools directory
                let custom_dir = if let Some(ref root) = ff_root {
                    root.join("crates")
                        .join("ff-agent")
                        .join("src")
                        .join("tools")
                } else {
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join(".forgefleet")
                        .join("custom_tools")
                };
                let _ = fs::create_dir_all(&custom_dir).await;

                let file_path = custom_dir.join(format!("{file_name}.rs"));
                match fs::write(&file_path, &source).await {
                    Ok(()) => {
                        let mut result = format!(
                            "Tool created: {struct_name}\n\
                             File: {}\n\
                             Tool name: {tool_name}\n\n\
                             To register this tool:\n\
                             1. Add `pub mod {file_name};` to tools/mod.rs\n\
                             2. Add `Box::new({file_name}::{struct_name})` to all_tools()\n\
                             3. Run `cargo build -p ff-terminal`\n\n\
                             Generated source:\n```rust\n{source}\n```",
                            file_path.display()
                        );

                        // Auto-rebuild if requested
                        if input
                            .get("rebuild")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            if let Some(ref root) = ff_root {
                                let build = tokio::process::Command::new("cargo")
                                    .args(["build", "-p", "ff-terminal"])
                                    .current_dir(root)
                                    .output()
                                    .await;
                                match build {
                                    Ok(out) if out.status.success() => {
                                        result.push_str("\n\nAuto-rebuild: SUCCESS");
                                    }
                                    Ok(out) => {
                                        result.push_str(&format!(
                                            "\n\nAuto-rebuild: FAILED\n{}",
                                            String::from_utf8_lossy(&out.stderr)
                                        ));
                                    }
                                    Err(e) => {
                                        result.push_str(&format!("\n\nAuto-rebuild: ERROR — {e}"));
                                    }
                                }
                            }
                        }

                        AgentToolResult::ok(truncate_output(&result, MAX_TOOL_RESULT_CHARS))
                    }
                    Err(e) => AgentToolResult::err(format!("Failed to write tool: {e}")),
                }
            }

            "preview" => {
                let struct_name = input
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("MyTool");
                let tool_name = input
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("MyTool");
                let description = input
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("A custom tool");

                AgentToolResult::ok(format!(
                    "Preview: Tool '{tool_name}' ({struct_name})\n\
                     Description: {description}\n\
                     File: tools/{}.rs\n\n\
                     This would generate a Rust source file with the AgentTool trait implementation.\n\
                     Use action='create' to actually create it.",
                    to_snake_case(struct_name)
                ))
            }

            "list_custom" => {
                let custom_dir = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".forgefleet")
                    .join("custom_tools");
                let mut tools = Vec::new();

                if let Ok(mut entries) = fs::read_dir(&custom_dir).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".rs") {
                            tools.push(format!("  {name}"));
                        }
                    }
                }

                // Also check the actual tools directory
                if let Some(ref root) = ff_root {
                    let tools_dir = root
                        .join("crates")
                        .join("ff-agent")
                        .join("src")
                        .join("tools");
                    if let Ok(mut entries) = fs::read_dir(&tools_dir).await {
                        while let Ok(Some(entry)) = entries.next_entry().await {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.ends_with(".rs") && name != "mod.rs" {
                                tools.push(format!("  [compiled] {name}"));
                            }
                        }
                    }
                }

                AgentToolResult::ok(format!(
                    "Tool files ({}):\n{}",
                    tools.len(),
                    tools.join("\n")
                ))
            }

            "delete" => {
                let struct_name = input.get("name").and_then(Value::as_str).unwrap_or("");
                if struct_name.is_empty() {
                    return AgentToolResult::err("'name' required");
                }
                let file_name = to_snake_case(struct_name);
                let custom_path = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".forgefleet")
                    .join("custom_tools")
                    .join(format!("{file_name}.rs"));
                match fs::remove_file(&custom_path).await {
                    Ok(()) => AgentToolResult::ok(format!("Deleted: {}", custom_path.display())),
                    Err(e) => AgentToolResult::err(format!("Failed to delete: {e}")),
                }
            }

            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

async fn find_forgefleet_root(ctx: &AgentToolContext) -> Option<std::path::PathBuf> {
    // Walk up from working dir to find Cargo.toml with forge-fleet
    let mut current = ctx.working_dir.clone();
    loop {
        let cargo = current.join("Cargo.toml");
        if let Ok(content) = fs::read_to_string(&cargo).await {
            if content.contains("forge-fleet") || content.contains("ff-agent") {
                return Some(current);
            }
        }
        if !current.pop() {
            break;
        }
    }
    None
}
