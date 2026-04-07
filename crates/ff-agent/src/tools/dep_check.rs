//! DependencyCheck tool — audit dependencies for vulnerabilities and updates.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct DepCheckTool;

#[async_trait]
impl AgentTool for DepCheckTool {
    fn name(&self) -> &str { "DepCheck" }
    fn description(&self) -> &str { "Audit project dependencies for security vulnerabilities and available updates." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["audit", "outdated", "tree"], "description": "Check action" }
            }
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("audit");
        let (cmd, args): (&str, Vec<&str>) = if ctx.working_dir.join("Cargo.toml").exists() {
            match action {
                "audit" => ("cargo", vec!["audit"]),
                "outdated" => ("cargo", vec!["outdated"]),
                "tree" => ("cargo", vec!["tree", "--depth", "1"]),
                _ => return AgentToolResult::err(format!("Unknown action: {action}")),
            }
        } else if ctx.working_dir.join("package.json").exists() {
            match action {
                "audit" => ("npm", vec!["audit"]),
                "outdated" => ("npm", vec!["outdated"]),
                "tree" => ("npm", vec!["ls", "--depth=0"]),
                _ => return AgentToolResult::err(format!("Unknown action: {action}")),
            }
        } else { return AgentToolResult::ok("No Cargo.toml or package.json found.".to_string()); };

        match Command::new(cmd).args(&args).current_dir(&ctx.working_dir).output().await {
            Ok(out) => {
                let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                AgentToolResult::ok(truncate_output(&combined, MAX_TOOL_RESULT_CHARS))
            }
            Err(e) => AgentToolResult::err(format!("{cmd} failed: {e}")),
        }
    }
}
