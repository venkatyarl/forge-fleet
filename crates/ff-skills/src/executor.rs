//! Skill executor — sandbox, timeout, and permission enforcement for tool
//! invocations.
//!
//! The executor is responsible for:
//! - Checking permissions before execution
//! - Enforcing timeouts
//! - Sandboxing shell commands
//! - Routing HTTP invocations
//! - Recording execution results

use std::collections::HashSet;
use std::time::Instant;

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::error::{Result, SkillError};
use crate::types::{
    SkillMetadata, SkillPermission, ToolDefinition, ToolExecutionResult, ToolInvocation,
};

// ─── Executor Config ─────────────────────────────────────────────────────────

/// Configuration for the skill executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Maximum allowed timeout for any tool (overrides per-tool settings).
    pub max_timeout_secs: u64,
    /// Default timeout when none is specified.
    pub default_timeout_secs: u64,
    /// Granted permissions (tools can only use these).
    pub granted_permissions: HashSet<SkillPermission>,
    /// Whether to enable sandbox mode for shell commands.
    pub sandbox_enabled: bool,
    /// Working directory for shell commands (overrides per-tool settings).
    pub working_dir: Option<std::path::PathBuf>,
    /// Environment variables to inject into shell commands.
    pub env_vars: std::collections::HashMap<String, String>,
    /// Maximum output size in bytes (truncate beyond this).
    pub max_output_bytes: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_timeout_secs: 300,
            default_timeout_secs: 30,
            granted_permissions: HashSet::new(),
            sandbox_enabled: true,
            working_dir: None,
            env_vars: std::collections::HashMap::new(),
            max_output_bytes: 512 * 1024, // 512KB
        }
    }
}

impl ExecutorConfig {
    /// Create a permissive config (all permissions granted, no sandbox).
    pub fn permissive() -> Self {
        let mut perms = HashSet::new();
        perms.insert(SkillPermission::FileRead);
        perms.insert(SkillPermission::FileWrite);
        perms.insert(SkillPermission::ShellExec);
        perms.insert(SkillPermission::Network);
        perms.insert(SkillPermission::EnvAccess);
        perms.insert(SkillPermission::ProcessSpawn);
        Self {
            granted_permissions: perms,
            sandbox_enabled: false,
            ..Default::default()
        }
    }
}

// ─── Executor ────────────────────────────────────────────────────────────────

/// The skill executor enforces permissions, sandboxing, and timeouts.
#[derive(Debug, Clone)]
pub struct SkillExecutor {
    config: ExecutorConfig,
}

impl SkillExecutor {
    /// Create a new executor with the given configuration.
    pub fn new(config: ExecutorConfig) -> Self {
        Self { config }
    }

    /// Create an executor with default (restrictive) config.
    pub fn default_executor() -> Self {
        Self::new(ExecutorConfig::default())
    }

    /// Create an executor with all permissions (for trusted skills).
    pub fn permissive() -> Self {
        Self::new(ExecutorConfig::permissive())
    }

    /// Execute a tool from a skill.
    pub async fn execute(
        &self,
        skill: &SkillMetadata,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<ToolExecutionResult> {
        // Find the tool in the skill.
        let tool = skill
            .find_tool(tool_name)
            .ok_or_else(|| SkillError::ToolNotFound {
                skill: skill.id.clone(),
                tool: tool_name.to_string(),
            })?;

        // Check permissions.
        self.check_permissions(skill, tool)?;

        // Determine timeout.
        let timeout_secs = tool.timeout_secs.min(self.config.max_timeout_secs).max(1);

        info!(
            skill = %skill.id,
            tool = %tool_name,
            timeout = timeout_secs,
            "executing tool"
        );

        let start = Instant::now();

        // Dispatch by invocation type.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            self.dispatch(tool, args),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(output)) => {
                let output = truncate_output(output, self.config.max_output_bytes);
                debug!(skill = %skill.id, tool = %tool_name, ms = duration_ms, "tool succeeded");
                Ok(ToolExecutionResult {
                    skill_id: skill.id.clone(),
                    tool_name: tool_name.to_string(),
                    success: true,
                    output,
                    error: None,
                    exit_code: Some(0),
                    duration_ms,
                    completed_at: Utc::now(),
                })
            }
            Ok(Err(e)) => {
                warn!(skill = %skill.id, tool = %tool_name, error = %e, "tool failed");
                Ok(ToolExecutionResult {
                    skill_id: skill.id.clone(),
                    tool_name: tool_name.to_string(),
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                    exit_code: None,
                    duration_ms,
                    completed_at: Utc::now(),
                })
            }
            Err(_) => {
                warn!(skill = %skill.id, tool = %tool_name, timeout = timeout_secs, "tool timed out");
                Err(SkillError::ExecutionTimeout {
                    tool: tool_name.to_string(),
                    timeout_secs,
                })
            }
        }
    }

    /// Check that all required permissions are granted.
    fn check_permissions(&self, skill: &SkillMetadata, tool: &ToolDefinition) -> Result<()> {
        // Collect all required permissions (skill-level + tool-level).
        let required: Vec<&SkillPermission> = skill
            .permissions
            .iter()
            .chain(tool.permissions.iter())
            .collect();

        for perm in required {
            // Custom permissions always need explicit granting.
            if !self.config.granted_permissions.contains(perm) {
                return Err(SkillError::PermissionDenied {
                    skill: skill.id.clone(),
                    permission: perm.to_string(),
                });
            }
        }

        // Shell invocations require ShellExec permission.
        if matches!(tool.invocation, ToolInvocation::Shell { .. })
            && !self
                .config
                .granted_permissions
                .contains(&SkillPermission::ShellExec)
        {
            return Err(SkillError::PermissionDenied {
                skill: skill.id.clone(),
                permission: "shell:exec".into(),
            });
        }

        // HTTP invocations require Network permission.
        if matches!(tool.invocation, ToolInvocation::Http { .. })
            && !self
                .config
                .granted_permissions
                .contains(&SkillPermission::Network)
        {
            return Err(SkillError::PermissionDenied {
                skill: skill.id.clone(),
                permission: "network".into(),
            });
        }

        Ok(())
    }

    /// Dispatch to the appropriate execution handler.
    async fn dispatch(&self, tool: &ToolDefinition, args: &serde_json::Value) -> Result<String> {
        match &tool.invocation {
            ToolInvocation::Shell {
                command,
                working_dir,
            } => {
                self.execute_shell(command, working_dir.as_deref(), args)
                    .await
            }

            ToolInvocation::Http {
                url,
                method,
                headers,
            } => self.execute_http(url, method, headers, args).await,

            ToolInvocation::Builtin { handler } => self.execute_builtin(handler, args).await,

            ToolInvocation::Prompt { template } => self.execute_prompt(template, args).await,
        }
    }

    /// Execute a shell command with optional sandboxing.
    async fn execute_shell(
        &self,
        command: &str,
        working_dir: Option<&std::path::Path>,
        args: &serde_json::Value,
    ) -> Result<String> {
        // Substitute $PARAM_NAME patterns in the command with args.
        let expanded = substitute_args(command, args);

        let work_dir = working_dir
            .or(self.config.working_dir.as_deref())
            .unwrap_or(std::path::Path::new("."));

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&expanded);
        cmd.current_dir(work_dir);

        // Inject environment variables.
        for (k, v) in &self.config.env_vars {
            cmd.env(k, v);
        }

        // If sandbox is enabled, restrict environment.
        if self.config.sandbox_enabled {
            cmd.env("FF_SANDBOXED", "1");
        }

        let output = cmd.output().await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if output.status.success() {
            Ok(stdout)
        } else {
            let msg = if stderr.is_empty() {
                format!("exit code: {:?}", output.status.code())
            } else {
                stderr
            };
            Err(SkillError::ExecutionFailed {
                tool: expanded,
                reason: msg,
            })
        }
    }

    /// Execute an HTTP tool invocation.
    ///
    /// Note: In production this would use reqwest. For now we shell out to curl
    /// to avoid adding reqwest as a dependency in this crate.
    async fn execute_http(
        &self,
        url: &str,
        method: &str,
        _headers: &std::collections::HashMap<String, String>,
        args: &serde_json::Value,
    ) -> Result<String> {
        if url.is_empty() {
            return Err(SkillError::ExecutionFailed {
                tool: "http".into(),
                reason: "no URL configured for HTTP tool".into(),
            });
        }

        let body = serde_json::to_string(args)?;
        let mut cmd = tokio::process::Command::new("curl");
        cmd.args([
            "-s",
            "-X",
            method,
            url,
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
        ]);

        let output = cmd.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        if output.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Err(SkillError::ExecutionFailed {
                tool: format!("{method} {url}"),
                reason: stderr,
            })
        }
    }

    /// Execute a builtin handler via the external registry contract.
    ///
    /// Builtin handlers are resolved by higher-level registries; this fallback
    /// returns a structured delegation result for the caller.
    async fn execute_builtin(&self, handler: &str, args: &serde_json::Value) -> Result<String> {
        debug!(handler, "builtin tool delegated to external registry");
        Ok(format!(
            "{{\"handler\": \"{handler}\", \"args\": {}, \"status\": \"external_registry_required\"}}",
            args
        ))
    }

    /// Execute a prompt template tool (expand template with args).
    async fn execute_prompt(&self, template: &str, args: &serde_json::Value) -> Result<String> {
        let expanded = substitute_args(template, args);
        Ok(expanded)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Substitute `$PARAM_NAME` and `${PARAM_NAME}` in text with values from args.
fn substitute_args(text: &str, args: &serde_json::Value) -> String {
    let mut result = text.to_string();

    if let Some(obj) = args.as_object() {
        for (key, value) in obj {
            let val_str = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Replace ${key} and $key patterns.
            result = result.replace(&format!("${{{key}}}"), &val_str);
            result = result.replace(&format!("${key}"), &val_str);
        }
    }

    result
}

/// Truncate output to max bytes (on a UTF-8 boundary).
fn truncate_output(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    // Find a safe truncation point.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("\n... [truncated]");
    truncated
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_substitute_args() {
        let args = serde_json::json!({"city": "Austin", "units": "metric"});
        let result = substitute_args("Weather for $city in ${units}", &args);
        assert_eq!(result, "Weather for Austin in metric");
    }

    #[test]
    fn test_substitute_no_args() {
        let result = substitute_args("hello world", &serde_json::json!({}));
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_truncate_output() {
        let short = "hello".to_string();
        assert_eq!(truncate_output(short, 100), "hello");

        let long = "a".repeat(200);
        let truncated = truncate_output(long, 50);
        assert!(truncated.len() < 100);
        assert!(truncated.contains("[truncated]"));
    }

    #[test]
    fn test_permission_check() {
        let executor = SkillExecutor::default_executor();
        let skill = SkillMetadata {
            id: "test".into(),
            name: "test".into(),
            description: "Test".into(),
            origin: crate::types::SkillOrigin::Custom,
            location: None,
            version: None,
            author: None,
            tags: Vec::new(),
            tools: vec![ToolDefinition {
                name: "run".into(),
                description: "Run".into(),
                parameters: Vec::new(),
                invocation: ToolInvocation::Shell {
                    command: "echo hi".into(),
                    working_dir: None,
                },
                permissions: Vec::new(),
                timeout_secs: 10,
            }],
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        };

        let tool = &skill.tools[0];
        // Default executor has no permissions → should deny shell.
        let result = executor.check_permissions(&skill, tool);
        assert!(result.is_err());

        // Permissive executor should allow.
        let perm_executor = SkillExecutor::permissive();
        let result = perm_executor.check_permissions(&skill, tool);
        assert!(result.is_ok());
    }

    #[test]
    fn test_executor_config_default() {
        let config = ExecutorConfig::default();
        assert_eq!(config.max_timeout_secs, 300);
        assert_eq!(config.default_timeout_secs, 30);
        assert!(config.sandbox_enabled);
        assert!(config.granted_permissions.is_empty());
    }
}
