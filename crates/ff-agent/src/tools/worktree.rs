//! Git worktree tools — create isolated working copies for safe parallel work.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult};

// ---------------------------------------------------------------------------
// EnterWorktree
// ---------------------------------------------------------------------------

pub struct EnterWorktreeTool;

#[async_trait]
impl AgentTool for EnterWorktreeTool {
    fn name(&self) -> &str {
        "EnterWorktree"
    }

    fn description(&self) -> &str {
        "Create a temporary git worktree to work in an isolated copy of the repository. Useful for making changes without affecting the main working directory."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "Optional branch name for the worktree (default: auto-generated)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let branch = input
            .get("branch")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("ff-worktree-{}", uuid::Uuid::new_v4().as_simple()));

        let worktree_path =
            std::env::temp_dir().join(format!("ff-wt-{}", &branch[..8.min(branch.len())]));

        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                &branch,
                &worktree_path.to_string_lossy(),
            ])
            .current_dir(&ctx.working_dir)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => AgentToolResult::ok(format!(
                "Created worktree at {}\nBranch: {branch}\nUse this as your working directory for isolated changes.",
                worktree_path.display()
            )),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                AgentToolResult::err(format!("Failed to create worktree: {stderr}"))
            }
            Err(e) => AgentToolResult::err(format!("Git command failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// ExitWorktree
// ---------------------------------------------------------------------------

pub struct ExitWorktreeTool;

#[async_trait]
impl AgentTool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "ExitWorktree"
    }

    fn description(&self) -> &str {
        "Remove a temporary git worktree and clean up. Optionally merge changes back."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "worktree_path": {
                    "type": "string",
                    "description": "Path to the worktree to remove"
                }
            },
            "required": ["worktree_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let path = match input.get("worktree_path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => return AgentToolResult::err("Missing 'worktree_path' parameter"),
        };

        let output = Command::new("git")
            .args(["worktree", "remove", "--force", path])
            .current_dir(&ctx.working_dir)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                AgentToolResult::ok(format!("Removed worktree at {path}"))
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                AgentToolResult::err(format!("Failed to remove worktree: {stderr}"))
            }
            Err(e) => AgentToolResult::err(format!("Git command failed: {e}")),
        }
    }
}
