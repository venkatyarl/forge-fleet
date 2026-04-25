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

        // Intercept bare SSH commands to fleet nodes and make them non-interactive
        let command = rewrite_fleet_ssh(command).await;
        let command = command.as_str();

        // Block interactive commands that would hang forever
        if is_interactive_command(command) {
            return AgentToolResult::err(format!(
                "Interactive command blocked: {command}\n\
                 This command opens an interactive session that would hang.\n\
                 Instead, include the command to run:\n\
                 - ssh user@host 'command here'\n\
                 - python3 -c 'print(1+1)'\n\
                 - mysql -e 'SELECT 1'"
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
    let output = Command::new("bash").arg("-c").arg(script).output().await?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    debug!(
        exit_code,
        stdout_len = stdout.len(),
        stderr_len = stderr.len(),
        "bash execution complete"
    );

    Ok((exit_code, stdout, stderr))
}

/// Fleet node name → (ip, ssh_user) lookup via Postgres.
async fn fleet_node_ip(name: &str) -> Option<(String, String)> {
    crate::fleet_info::fetch_node_ip_user(name).await
}

/// Rewrite bare SSH commands to fleet nodes into non-interactive commands.
/// e.g. "ssh <node>" → "ssh -o ConnectTimeout=10 <user>@<ip> 'hostname && uptime && ...'"
/// where `<ip>` and `<user>` are resolved from Postgres via `fleet_node_ip`.
async fn rewrite_fleet_ssh(command: &str) -> String {
    let trimmed = command.trim();

    // Match "ssh <nodename>" with no additional command
    if trimmed.starts_with("ssh ") {
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() == 2 {
            let target = parts[1];
            // Check if target is a fleet node name (no @ sign, no IP)
            if !target.contains('@') && !target.contains('.') {
                if let Some((ip, user)) = fleet_node_ip(target).await {
                    return format!(
                        "ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=no {user}@{ip} 'echo \"=== {target} ({ip}) ===\"  && hostname && echo \"---\" && uptime && echo \"---\" && uname -sr && echo \"---\" && free -h 2>/dev/null || sysctl -n hw.memsize 2>/dev/null && echo \"---\" && df -h / && echo \"---\" && echo \"Running processes:\" && ps aux --sort=-%cpu 2>/dev/null | head -6 || ps aux | head -6'"
                    );
                }
            }
        }
    }

    // Match "ssh into <nodename>" pattern
    if trimmed.starts_with("ssh into ") || trimmed.starts_with("ssh to ") {
        let node_name = trimmed.split_whitespace().last().unwrap_or("");
        if let Some((ip, user)) = fleet_node_ip(node_name).await {
            return format!(
                "ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=no {user}@{ip} 'echo \"=== {node_name} ({ip}) ===\" && hostname && uptime && uname -sr'"
            );
        }
    }

    command.to_string()
}

/// Detect commands that would open an interactive session and hang.
fn is_interactive_command(command: &str) -> bool {
    let trimmed = command.trim();

    // Bare SSH without a command (already handled by rewrite, but catch edge cases)
    if trimmed == "ssh" {
        return true;
    }

    // Interactive interpreters without -c flag
    let interactive = [
        "python3",
        "python",
        "node",
        "irb",
        "ghci",
        "lua",
        "mysql",
        "psql",
        "sqlite3",
        "redis-cli",
        "mongo",
        "vim",
        "vi",
        "nano",
        "emacs",
        "less",
        "more",
        "top",
        "htop",
        "bash",
        "zsh",
        "sh",
        "fish",
    ];

    // Only block if it's the bare command with no arguments
    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    let word_count = trimmed.split_whitespace().count();

    if word_count == 1 && interactive.contains(&first_word) {
        return true;
    }

    false
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
