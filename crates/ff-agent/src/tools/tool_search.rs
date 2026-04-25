//! ToolSearch — lets the agent discover and learn about available tools.
//!
//! Instead of sending all 129 tool definitions to the LLM (which would consume
//! the entire context window), the agent gets a small core set + this ToolSearch
//! tool. When it needs a capability it doesn't have, it searches for the right
//! tool and gets the full schema back.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, all_tools};

pub struct ToolSearchTool;

#[async_trait]
impl AgentTool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        "Search for available tools by name or capability. Use this when you need a tool \
         that isn't in your core set. Returns matching tool names with descriptions and \
         parameter schemas. You have 129 tools available across categories: \
         File Ops, Git, Tasks, Web, Fleet, Database, Docker, Code Quality, Finance, \
         Media, Research, Security, Analytics, Networking, and more."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query — tool name, category, or capability description (e.g. 'git', 'docker', 'database', 'screenshot', 'cron')"
                },
                "list_categories": {
                    "type": "boolean",
                    "description": "If true, list all tool categories with counts instead of searching"
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let list_categories = input
            .get("list_categories")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if list_categories {
            return AgentToolResult::ok(list_all_categories());
        }

        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();

        if query.is_empty() {
            return AgentToolResult::ok(list_all_categories());
        }

        let tools = all_tools();
        let mut matches: Vec<(String, String, Value)> = Vec::new();

        for tool in &tools {
            let name = tool.name().to_ascii_lowercase();
            let desc = tool.description().to_ascii_lowercase();
            let schema = tool.parameters_schema();

            if name.contains(&query) || desc.contains(&query) {
                matches.push((
                    tool.name().to_string(),
                    tool.description().to_string(),
                    schema,
                ));
            }
        }

        if matches.is_empty() {
            return AgentToolResult::ok(format!(
                "No tools found matching '{}'. Try broader terms like 'git', 'file', 'web', 'fleet', 'docker', 'database', 'media'.",
                query
            ));
        }

        let mut result = format!("Found {} tool(s) matching '{}':\n\n", matches.len(), query);
        for (name, desc, schema) in &matches {
            result.push_str(&format!("### {}\n{}\n", name, desc));
            result.push_str(&format!(
                "Parameters: {}\n\n",
                serde_json::to_string_pretty(schema).unwrap_or_default()
            ));
        }

        // Truncate if too large (keep under 4KB to not blow context)
        if result.len() > 4000 {
            result.truncate(3900);
            result.push_str("\n\n...[truncated — narrow your search]");
        }

        AgentToolResult::ok(result)
    }
}

fn list_all_categories() -> String {
    let tools = all_tools();
    let mut categories: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for tool in &tools {
        // Infer category from module/description
        let name = tool.name().to_string();
        let desc = tool.description().to_ascii_lowercase();

        let category = if desc.contains("git") || name.starts_with("Git") {
            "Git & Version Control"
        } else if desc.contains("docker") || name.contains("Docker") {
            "Docker & Containers"
        } else if desc.contains("database") || desc.contains("sql") {
            "Database"
        } else if desc.contains("task") {
            "Task Management"
        } else if desc.contains("web") || desc.contains("fetch") || desc.contains("search") {
            "Web & Research"
        } else if desc.contains("fleet") || desc.contains("ssh") || desc.contains("node") {
            "Fleet Operations"
        } else if desc.contains("file")
            || desc.contains("read")
            || desc.contains("write")
            || desc.contains("edit")
        {
            "File Operations"
        } else if desc.contains("shell") || desc.contains("bash") || desc.contains("command") {
            "System & Shell"
        } else if desc.contains("cron") || desc.contains("schedule") {
            "Scheduling"
        } else if desc.contains("model") || desc.contains("llm") {
            "Model Management"
        } else if desc.contains("agent") || desc.contains("orchestrat") {
            "Agent & Orchestration"
        } else if desc.contains("image")
            || desc.contains("video")
            || desc.contains("media")
            || desc.contains("photo")
            || desc.contains("screenshot")
        {
            "Media & Vision"
        } else if desc.contains("lint") || desc.contains("quality") || desc.contains("security") {
            "Code Quality & Security"
        } else if desc.contains("finance") || desc.contains("money") || desc.contains("budget") {
            "Finance"
        } else if desc.contains("network") || desc.contains("http") || desc.contains("api") {
            "Networking & APIs"
        } else if desc.contains("crypto") || desc.contains("encrypt") || desc.contains("hash") {
            "Crypto & Security"
        } else if desc.contains("doc") || desc.contains("notebook") {
            "Documentation"
        } else {
            "Utility"
        };

        categories
            .entry(category.to_string())
            .or_default()
            .push(name);
    }

    let mut result = format!(
        "ForgeFleet has {} tools across {} categories:\n\n",
        tools.len(),
        categories.len()
    );
    for (category, names) in &categories {
        result.push_str(&format!(
            "**{}** ({}):\n  {}\n\n",
            category,
            names.len(),
            names.join(", ")
        ));
    }
    result.push_str(
        "Use ToolSearch with a query to get full details and parameter schemas for any tool.",
    );
    result
}
