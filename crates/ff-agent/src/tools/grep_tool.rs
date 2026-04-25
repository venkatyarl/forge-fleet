//! Grep tool — search file contents using ripgrep.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct GrepTool;

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents using regex patterns. Uses ripgrep (rg) under the hood. Supports file type filtering, glob patterns, context lines, and multiple output modes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default: working directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. '*.rs', '*.{ts,tsx}')"
                },
                "type": {
                    "type": "string",
                    "description": "File type to search (e.g. 'rust', 'js', 'py')"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default: files_with_matches)"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default false)"
                },
                "context": {
                    "type": "number",
                    "description": "Number of context lines before and after each match"
                },
                "head_limit": {
                    "type": "number",
                    "description": "Limit output to first N results (default 250)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let pattern = match input.get("pattern").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => return AgentToolResult::err("Missing or empty 'pattern' parameter"),
        };

        let search_path = input
            .get("path")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let output_mode = input
            .get("output_mode")
            .and_then(Value::as_str)
            .unwrap_or("files_with_matches");

        let head_limit = input
            .get("head_limit")
            .and_then(Value::as_u64)
            .unwrap_or(250) as usize;

        // Check if rg is available, fall back to grep
        let rg_available = Command::new("which")
            .arg("rg")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        let mut cmd = if rg_available {
            let mut cmd = Command::new("rg");
            cmd.arg("--no-heading");
            cmd.arg("--color=never");

            match output_mode {
                "files_with_matches" => {
                    cmd.arg("-l");
                }
                "count" => {
                    cmd.arg("-c");
                }
                _ => {
                    cmd.arg("-n"); // line numbers
                }
            }

            if input
                .get("case_insensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                cmd.arg("-i");
            }

            if let Some(context) = input.get("context").and_then(Value::as_u64) {
                cmd.arg("-C").arg(context.to_string());
            }

            if let Some(glob) = input.get("glob").and_then(Value::as_str) {
                cmd.arg("--glob").arg(glob);
            }

            if let Some(file_type) = input.get("type").and_then(Value::as_str) {
                cmd.arg("--type").arg(file_type);
            }

            cmd.arg("--").arg(pattern).arg(&search_path);
            cmd
        } else {
            // Fallback to grep
            let mut cmd = Command::new("grep");
            cmd.arg("-r").arg("-n").arg("--color=never");

            if output_mode == "files_with_matches" {
                cmd.arg("-l");
            } else if output_mode == "count" {
                cmd.arg("-c");
            }

            if input
                .get("case_insensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                cmd.arg("-i");
            }

            cmd.arg("-e").arg(pattern).arg(&search_path);
            cmd
        };

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return AgentToolResult::err(format!("Failed to run search: {e}")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.trim().is_empty() {
            return AgentToolResult::ok(format!("No matches found for pattern: {pattern}"));
        }

        // Apply head limit
        let lines: Vec<&str> = stdout.lines().take(head_limit).collect();
        let total_lines = stdout.lines().count();
        let mut result = lines.join("\n");

        if total_lines > head_limit {
            result.push_str(&format!(
                "\n\n... ({} more results, {total_lines} total)",
                total_lines - head_limit
            ));
        }

        AgentToolResult::ok(truncate_output(&result, MAX_TOOL_RESULT_CHARS))
    }
}
