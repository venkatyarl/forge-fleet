//! SkillBuilder — create, install, test, and manage custom skills at runtime.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult};

/// SkillBuilder — create new skills from natural language descriptions.
pub struct SkillBuilderTool;

#[async_trait]
impl AgentTool for SkillBuilderTool {
    fn name(&self) -> &str {
        "SkillBuilder"
    }
    fn description(&self) -> &str {
        "Create, install, list, or test custom skills. Skills are reusable tool definitions stored as SKILL.md files that extend ForgeFleet's capabilities."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["create","install","list","test","delete","show"],"description":"What to do"},
            "name":{"type":"string","description":"Skill name (for create/install/delete/show)"},
            "description":{"type":"string","description":"What the skill does (for create)"},
            "command":{"type":"string","description":"Shell command template with $PARAM placeholders (for create)"},
            "parameters":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"type":{"type":"string"},"description":{"type":"string"},"required":{"type":"boolean"}}},"description":"Parameters for the skill (for create)"},
            "url":{"type":"string","description":"URL to install skill from (for install)"},
            "test_input":{"type":"object","description":"Test input parameters (for test)"},
            "scope":{"type":"string","enum":["global","project"],"description":"Where to save (default: project)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let scope = input
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("project");

        let skills_dir = if scope == "global" {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".forgefleet")
                .join("skills")
        } else {
            ctx.working_dir.join(".forgefleet").join("skills")
        };

        match action {
            "create" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                let description = input
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let command = input.get("command").and_then(Value::as_str).unwrap_or("");

                if name.is_empty() || description.is_empty() {
                    return AgentToolResult::err("'name' and 'description' required for create");
                }

                let skill_dir = skills_dir.join(name);
                let _ = fs::create_dir_all(&skill_dir).await;

                // Build SKILL.md
                let mut skill_md = format!(
                    "# {name}\n\n{description}\n\n## Tools\n\n### {name}\n{description}\n\n"
                );

                // Add parameters
                if let Some(params) = input.get("parameters").and_then(Value::as_array) {
                    skill_md.push_str("**Parameters:**\n");
                    for param in params {
                        let pname = param.get("name").and_then(Value::as_str).unwrap_or("param");
                        let ptype = param
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("string");
                        let pdesc = param
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let required = param
                            .get("required")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        skill_md.push_str(&format!(
                            "- `{pname}` ({ptype}{}): {pdesc}\n",
                            if required { ", required" } else { "" }
                        ));
                    }
                    skill_md.push('\n');
                }

                // Add command
                if !command.is_empty() {
                    skill_md.push_str(&format!("**Command:**\n```bash\n{command}\n```\n"));
                }

                let skill_path = skill_dir.join("SKILL.md");
                match fs::write(&skill_path, &skill_md).await {
                    Ok(()) => AgentToolResult::ok(format!(
                        "Skill created: {name}\nPath: {}\nScope: {scope}\n\nSKILL.md:\n{skill_md}",
                        skill_path.display()
                    )),
                    Err(e) => AgentToolResult::err(format!("Failed to create skill: {e}")),
                }
            }

            "install" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                let url = input.get("url").and_then(Value::as_str).unwrap_or("");

                if !url.is_empty() {
                    // Download from URL
                    let client = reqwest::Client::new();
                    match client.get(url).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            let content = resp.text().await.unwrap_or_default();
                            let skill_dir = skills_dir.join(if name.is_empty() {
                                "downloaded-skill"
                            } else {
                                name
                            });
                            let _ = fs::create_dir_all(&skill_dir).await;
                            let _ = fs::write(skill_dir.join("SKILL.md"), &content).await;
                            AgentToolResult::ok(format!("Skill installed from {url}"))
                        }
                        _ => AgentToolResult::err(format!("Failed to download skill from {url}")),
                    }
                } else {
                    AgentToolResult::err(
                        "Provide 'url' to install from, or use 'create' to make a new skill"
                            .to_string(),
                    )
                }
            }

            "list" => {
                let mut skills = Vec::new();

                // Global skills
                let global_dir = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".forgefleet")
                    .join("skills");
                if let Ok(mut entries) = fs::read_dir(&global_dir).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if entry.path().is_dir() {
                            skills.push(format!(
                                "  [global] {}",
                                entry.file_name().to_string_lossy()
                            ));
                        }
                    }
                }

                // Project skills
                let project_dir = ctx.working_dir.join(".forgefleet").join("skills");
                if let Ok(mut entries) = fs::read_dir(&project_dir).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if entry.path().is_dir() {
                            skills.push(format!(
                                "  [project] {}",
                                entry.file_name().to_string_lossy()
                            ));
                        }
                    }
                }

                if skills.is_empty() {
                    AgentToolResult::ok(
                        "No custom skills installed. Use SkillBuilder create to make one."
                            .to_string(),
                    )
                } else {
                    AgentToolResult::ok(format!(
                        "Custom Skills ({}):\n{}",
                        skills.len(),
                        skills.join("\n")
                    ))
                }
            }

            "show" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return AgentToolResult::err("'name' required");
                }

                for dir in [
                    &skills_dir,
                    &dirs::home_dir()
                        .unwrap_or_default()
                        .join(".forgefleet")
                        .join("skills"),
                ] {
                    let skill_path = dir.join(name).join("SKILL.md");
                    if let Ok(content) = fs::read_to_string(&skill_path).await {
                        return AgentToolResult::ok(format!(
                            "Skill: {name}\nPath: {}\n\n{content}",
                            skill_path.display()
                        ));
                    }
                }
                AgentToolResult::err(format!("Skill '{name}' not found"))
            }

            "test" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return AgentToolResult::err("'name' required for test");
                }
                AgentToolResult::ok(format!(
                    "Skill test for '{name}': Use Bash tool to run the skill's command with test parameters."
                ))
            }

            "delete" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return AgentToolResult::err("'name' required");
                }
                let skill_dir = skills_dir.join(name);
                match fs::remove_dir_all(&skill_dir).await {
                    Ok(()) => AgentToolResult::ok(format!("Skill '{name}' deleted")),
                    Err(e) => AgentToolResult::err(format!("Failed to delete skill: {e}")),
                }
            }

            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}
