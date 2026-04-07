//! Diff tool — generate diffs between files or git versions.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct DiffTool;

#[async_trait]
impl AgentTool for DiffTool {
    fn name(&self) -> &str { "Diff" }

    fn description(&self) -> &str {
        "Generate diffs: between two files, between git versions, or show working directory changes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["files", "git", "staged", "working"],
                    "description": "Diff mode: files (two paths), git (commit range), staged (git staged), working (unstaged)"
                },
                "file_a": { "type": "string", "description": "First file path (for files mode)" },
                "file_b": { "type": "string", "description": "Second file path (for files mode)" },
                "range": { "type": "string", "description": "Git commit range (e.g. 'HEAD~3..HEAD' or branch name)" }
            },
            "required": ["mode"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let mode = input.get("mode").and_then(Value::as_str).unwrap_or("working");

        let output = match mode {
            "files" => {
                let a = input.get("file_a").and_then(Value::as_str).unwrap_or("");
                let b = input.get("file_b").and_then(Value::as_str).unwrap_or("");
                if a.is_empty() || b.is_empty() { return AgentToolResult::err("Both 'file_a' and 'file_b' required"); }
                Command::new("diff").args(["-u", a, b]).current_dir(&ctx.working_dir).output().await
            }
            "git" => {
                let range = input.get("range").and_then(Value::as_str).unwrap_or("HEAD~1..HEAD");
                Command::new("git").args(["diff", range]).current_dir(&ctx.working_dir).output().await
            }
            "staged" => {
                Command::new("git").args(["diff", "--staged"]).current_dir(&ctx.working_dir).output().await
            }
            "working" => {
                Command::new("git").args(["diff"]).current_dir(&ctx.working_dir).output().await
            }
            _ => return AgentToolResult::err(format!("Unknown diff mode: {mode}")),
        };

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.trim().is_empty() {
                    AgentToolResult::ok("No differences found.")
                } else {
                    AgentToolResult::ok(truncate_output(&stdout, MAX_TOOL_RESULT_CHARS))
                }
            }
            Err(e) => AgentToolResult::err(format!("Diff failed: {e}")),
        }
    }
}
