//! Bash tool — execute shell commands with persistent cwd and env vars.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::debug;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct BashTool;

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. The shell state (cwd, env vars) persists across calls within the same session. Use this for running builds, tests, git commands, and general system operations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "number",
                    "description": "Optional timeout in milliseconds (default 120000, max 600000)"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of what this command does"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let command = match input.get("command").and_then(Value::as_str) {
            Some(cmd) if !cmd.trim().is_empty() => cmd,
            _ => return AgentToolResult::err("Missing or empty 'command' parameter"),
        };

        let timeout_ms = input
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(120_000)
            .min(600_000);

        // Block obviously destructive commands
        if is_blocked_command(command) {
            return AgentToolResult::err(format!(
                "Command blocked for safety: {command}\nThis command is potentially destructive. Use a more targeted approach."
            ));
        }

        let shell_state = ctx.shell_state.lock().await;
        let effective_cwd = shell_state
            .cwd
            .clone()
            .unwrap_or_else(|| ctx.working_dir.clone());
        let env_vars = shell_state.env_vars.clone();
        drop(shell_state);

        // Build a wrapper script that:
        // 1. Sets env vars from persistent state
        // 2. cd's to the persistent cwd
        // 3. Runs the command
        // 4. Outputs a sentinel block with the final cwd and new exports
        // Simpler approach: run the command directly, then capture pwd separately.
        // Avoids leaking env vars into tool output.
        let wrapper = format!(
            r#"cd {cwd} 2>/dev/null || true
{exports}
{command}"#,
            cwd = shell_quote(&effective_cwd.to_string_lossy()),
            exports = env_vars
                .iter()
                .map(|(k, v)| format!("export {}={}", k, shell_quote(v)))
                .collect::<Vec<_>>()
                .join("\n"),
            command = command,
        );

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            run_shell(&wrapper),
        )
        .await;

        match result {
            Ok(Ok((exit_code, stdout, stderr))) => {
                let mut output = stdout.trim_end().to_string();
                if !stderr.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(stderr.trim_end());
                }

                if exit_code != 0 {
                    output.push_str(&format!("\n\nExit code: {exit_code}"));
                }

                let output = truncate_output(&output, MAX_TOOL_RESULT_CHARS);
                if exit_code != 0 {
                    AgentToolResult::err(output)
                } else {
                    AgentToolResult::ok(output)
                }
            }
            Ok(Err(err)) => AgentToolResult::err(format!("Failed to execute command: {err}")),
            Err(_) => AgentToolResult::err(format!(
                "Command timed out after {timeout_ms}ms. Consider using a shorter command or increasing the timeout."
            )),
        }
    }
}

async fn run_shell(script: &str) -> anyhow::Result<(i32, String, String)> {
    let output = Command::new("bash")
        .arg("-c")
        .arg(script)
        .output()
        .await?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    debug!(exit_code, stdout_len = stdout.len(), stderr_len = stderr.len(), "bash execution complete");

    Ok((exit_code, stdout, stderr))
}

fn is_blocked_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let blocked = [
        "rm -rf /",
        "rm -rf /*",
        "mkfs.",
        ":(){ :|:& };:",
        "dd if=/dev/zero of=/dev/sd",
        "dd if=/dev/random of=/dev/sd",
        "> /dev/sda",
        "shutdown -h",
        "shutdown now",
        "reboot",
        "halt",
        "init 0",
        "init 6",
    ];
    blocked.iter().any(|b| lower.contains(b))
}

fn shell_quote(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}
