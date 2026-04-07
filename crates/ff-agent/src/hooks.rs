//! Hook system — execute user-defined scripts at key agent lifecycle points.
//!
//! Hooks run shell commands before/after tool execution, after model responses,
//! and on session end. Configured in fleet.toml or per-project config.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, warn};

/// Hook lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool is executed.
    PreToolUse,
    /// After a tool finishes.
    PostToolUse,
    /// After the model produces a response (each turn).
    PostModelTurn,
    /// When the agent session ends.
    Stop,
    /// Before a user message is sent to the LLM.
    UserPromptSubmit,
    /// When a notification should be sent.
    Notification,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PostModelTurn => "post_model_turn",
            Self::Stop => "stop",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::Notification => "notification",
        }
    }
}

/// A configured hook entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    /// Shell command to execute.
    pub command: String,
    /// Optional tool name filter (only trigger for specific tools).
    #[serde(default)]
    pub tool_filter: Option<String>,
    /// Timeout in seconds (default 10).
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// If true, hook failure blocks the action (for PreToolUse).
    #[serde(default)]
    pub blocking: bool,
}

fn default_timeout() -> u64 { 10 }

/// Hook configuration for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookConfig {
    pub hooks: HashMap<HookEvent, Vec<HookEntry>>,
}

impl HookConfig {
    pub fn new() -> Self {
        Self { hooks: HashMap::new() }
    }

    pub fn add(&mut self, event: HookEvent, entry: HookEntry) {
        self.hooks.entry(event).or_default().push(entry);
    }
}

/// Result of running hooks for an event.
#[derive(Debug)]
pub struct HookResult {
    /// Whether any blocking hook failed (action should be prevented).
    pub blocked: bool,
    /// Blocking error messages (if any).
    pub block_reasons: Vec<String>,
    /// Number of hooks executed.
    pub executed: usize,
}

/// Run all hooks for a given event.
pub async fn run_hooks(
    config: &HookConfig,
    event: HookEvent,
    env_vars: &HashMap<String, String>,
    working_dir: &Path,
) -> HookResult {
    let entries = match config.hooks.get(&event) {
        Some(e) if !e.is_empty() => e,
        _ => {
            return HookResult {
                blocked: false,
                block_reasons: vec![],
                executed: 0,
            };
        }
    };

    let mut result = HookResult {
        blocked: false,
        block_reasons: vec![],
        executed: 0,
    };

    for entry in entries {
        // Apply tool filter if present
        if let Some(filter) = &entry.tool_filter {
            if let Some(tool_name) = env_vars.get("FORGEFLEET_TOOL_NAME") {
                if !tool_name.eq_ignore_ascii_case(filter) {
                    continue;
                }
            }
        }

        let timeout = std::time::Duration::from_secs(entry.timeout_secs);
        let cmd_result = tokio::time::timeout(timeout, run_hook_command(&entry.command, env_vars, working_dir)).await;

        result.executed += 1;

        match cmd_result {
            Ok(Ok(exit_code)) => {
                if exit_code != 0 && entry.blocking {
                    result.blocked = true;
                    result.block_reasons.push(format!(
                        "Hook '{}' exited with code {exit_code}",
                        entry.command
                    ));
                }
                debug!(
                    event = event.as_str(),
                    command = %entry.command,
                    exit_code,
                    "hook executed"
                );
            }
            Ok(Err(e)) => {
                warn!(event = event.as_str(), command = %entry.command, error = %e, "hook failed");
                if entry.blocking {
                    result.blocked = true;
                    result.block_reasons.push(format!("Hook '{}' failed: {e}", entry.command));
                }
            }
            Err(_) => {
                warn!(event = event.as_str(), command = %entry.command, "hook timed out");
                if entry.blocking {
                    result.blocked = true;
                    result.block_reasons.push(format!(
                        "Hook '{}' timed out after {}s",
                        entry.command, entry.timeout_secs
                    ));
                }
            }
        }
    }

    result
}

async fn run_hook_command(
    command: &str,
    env_vars: &HashMap<String, String>,
    working_dir: &Path,
) -> anyhow::Result<i32> {
    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .envs(env_vars)
        .current_dir(working_dir)
        .output()
        .await?;

    Ok(output.status.code().unwrap_or(-1))
}
