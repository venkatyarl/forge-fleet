//! DocGen tool — generate documentation from code.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct DocGenTool;

#[async_trait]
impl AgentTool for DocGenTool {
    fn name(&self) -> &str {
        "DocGen"
    }
    fn description(&self) -> &str {
        "Generate documentation: rustdoc, JSDoc, Python docstrings, or markdown docs from code."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["build", "check", "open"], "description": "Doc action" },
                "format": { "type": "string", "enum": ["auto", "rustdoc", "jsdoc", "sphinx"], "description": "Doc format" }
            }
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("build");
        // Auto-detect project type
        if ctx.working_dir.join("Cargo.toml").exists() {
            let args = match action {
                "check" => vec!["doc", "--no-deps"],
                _ => vec!["doc", "--no-deps", "--open"],
            };
            match Command::new("cargo")
                .args(&args)
                .current_dir(&ctx.working_dir)
                .output()
                .await
            {
                Ok(out) => {
                    let combined = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    if out.status.success() {
                        AgentToolResult::ok(truncate_output(&combined, MAX_TOOL_RESULT_CHARS))
                    } else {
                        AgentToolResult::err(truncate_output(&combined, MAX_TOOL_RESULT_CHARS))
                    }
                }
                Err(e) => AgentToolResult::err(format!("cargo doc failed: {e}")),
            }
        } else if ctx.working_dir.join("package.json").exists() {
            AgentToolResult::ok("JSDoc generation: run `npx jsdoc -c jsdoc.json` or use the Bash tool to run your project's doc command.".to_string())
        } else {
            AgentToolResult::ok("Auto-detect: no Cargo.toml or package.json found. Use Bash to run your project's documentation command.".to_string())
        }
    }
}
