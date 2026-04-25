//! GitPR tool — create, review, and manage pull requests via GitHub CLI.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct GitPRTool;

#[async_trait]
impl AgentTool for GitPRTool {
    fn name(&self) -> &str {
        "GitPR"
    }

    fn description(&self) -> &str {
        "Manage GitHub pull requests using the gh CLI. Create PRs, list PRs, review, merge, and check PR status."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "view", "merge", "diff", "checks", "review"],
                    "description": "PR action to perform"
                },
                "title": { "type": "string", "description": "PR title (for create)" },
                "body": { "type": "string", "description": "PR body/description (for create)" },
                "pr_number": { "type": "number", "description": "PR number (for view/merge/diff/checks/review)" },
                "base": { "type": "string", "description": "Base branch (for create, default main)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");

        let mut cmd = Command::new("gh");
        cmd.current_dir(&ctx.working_dir);

        match action {
            "create" => {
                cmd.arg("pr").arg("create");
                if let Some(title) = input.get("title").and_then(Value::as_str) {
                    cmd.arg("--title").arg(title);
                }
                if let Some(body) = input.get("body").and_then(Value::as_str) {
                    cmd.arg("--body").arg(body);
                }
                if let Some(base) = input.get("base").and_then(Value::as_str) {
                    cmd.arg("--base").arg(base);
                }
            }
            "list" => {
                cmd.arg("pr").arg("list");
            }
            "view" => {
                cmd.arg("pr").arg("view");
                if let Some(n) = input.get("pr_number").and_then(Value::as_u64) {
                    cmd.arg(n.to_string());
                }
            }
            "merge" => {
                cmd.arg("pr").arg("merge");
                if let Some(n) = input.get("pr_number").and_then(Value::as_u64) {
                    cmd.arg(n.to_string());
                }
                cmd.arg("--auto").arg("--squash");
            }
            "diff" => {
                cmd.arg("pr").arg("diff");
                if let Some(n) = input.get("pr_number").and_then(Value::as_u64) {
                    cmd.arg(n.to_string());
                }
            }
            "checks" => {
                cmd.arg("pr").arg("checks");
                if let Some(n) = input.get("pr_number").and_then(Value::as_u64) {
                    cmd.arg(n.to_string());
                }
            }
            "review" => {
                cmd.arg("pr").arg("review");
                if let Some(n) = input.get("pr_number").and_then(Value::as_u64) {
                    cmd.arg(n.to_string());
                }
                cmd.arg("--approve");
            }
            _ => return AgentToolResult::err(format!("Unknown PR action: {action}")),
        }

        match cmd.output().await {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{stdout}{stderr}");
                if out.status.success() {
                    AgentToolResult::ok(truncate_output(&combined, MAX_TOOL_RESULT_CHARS))
                } else {
                    AgentToolResult::err(truncate_output(&combined, MAX_TOOL_RESULT_CHARS))
                }
            }
            Err(e) => {
                AgentToolResult::err(format!("gh command failed: {e}. Is GitHub CLI installed?"))
            }
        }
    }
}
