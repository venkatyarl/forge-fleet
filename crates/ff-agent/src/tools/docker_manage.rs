//! DockerManage tool — manage Docker containers on fleet nodes.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct DockerManageTool;

#[async_trait]
impl AgentTool for DockerManageTool {
    fn name(&self) -> &str {
        "Docker"
    }

    fn description(&self) -> &str {
        "Manage Docker containers: list, start, stop, build, run, logs, and compose operations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["ps", "images", "build", "run", "stop", "rm", "logs", "compose-up", "compose-down", "exec"],
                    "description": "Docker action"
                },
                "target": { "type": "string", "description": "Container name/ID, image name, or compose file path" },
                "args": { "type": "string", "description": "Additional arguments" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let target = input.get("target").and_then(Value::as_str).unwrap_or("");
        let extra_args = input.get("args").and_then(Value::as_str).unwrap_or("");

        let mut cmd = Command::new("docker");
        cmd.current_dir(&ctx.working_dir);

        match action {
            "ps" => {
                cmd.args(["ps", "-a"]);
            }
            "images" => {
                cmd.arg("images");
            }
            "build" => {
                cmd.args(["build", "-t", target, "."]);
            }
            "run" => {
                cmd.arg("run").arg("-d");
                if !extra_args.is_empty() {
                    for arg in extra_args.split_whitespace() {
                        cmd.arg(arg);
                    }
                }
                cmd.arg(target);
            }
            "stop" => {
                cmd.args(["stop", target]);
            }
            "rm" => {
                cmd.args(["rm", "-f", target]);
            }
            "logs" => {
                cmd.args(["logs", "--tail", "100", target]);
            }
            "compose-up" => {
                cmd = Command::new("docker");
                cmd.current_dir(&ctx.working_dir);
                cmd.args(["compose"]);
                if !target.is_empty() {
                    cmd.args(["-f", target]);
                }
                cmd.args(["up", "-d"]);
            }
            "compose-down" => {
                cmd = Command::new("docker");
                cmd.current_dir(&ctx.working_dir);
                cmd.args(["compose"]);
                if !target.is_empty() {
                    cmd.args(["-f", target]);
                }
                cmd.arg("down");
            }
            "exec" => {
                cmd.args(["exec", target, "bash", "-c", extra_args]);
            }
            _ => return AgentToolResult::err(format!("Unknown docker action: {action}")),
        }

        match cmd.output().await {
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
            Err(e) => AgentToolResult::err(format!("Docker command failed: {e}")),
        }
    }
}
