use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::SshNodeConfig;
use crate::connection::{SshConnection, SshConnectionError, SshConnectionOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCommandResult {
    pub node: String,
    pub host: String,
    pub command: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u128,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanoutCommandResult {
    pub command: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub results: Vec<NodeCommandResult>,
}

impl FanoutCommandResult {
    pub fn success_count(&self) -> usize {
        self.results.iter().filter(|r| r.success).count()
    }

    pub fn failure_count(&self) -> usize {
        self.results.len().saturating_sub(self.success_count())
    }
}

#[derive(Debug, Error)]
pub enum RemoteExecError {
    #[error("ssh transport error: {0}")]
    Ssh(#[from] SshConnectionError),

    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// High-level remote command execution helper.
#[derive(Debug, Clone)]
pub struct RemoteExecutor {
    command_timeout_secs: u64,
    batch_mode: bool,
}

impl Default for RemoteExecutor {
    fn default() -> Self {
        Self {
            command_timeout_secs: 60,
            batch_mode: true,
        }
    }
}

impl RemoteExecutor {
    pub fn new(command_timeout_secs: u64, batch_mode: bool) -> Self {
        Self {
            command_timeout_secs: command_timeout_secs.max(1),
            batch_mode,
        }
    }

    /// Run a command on one node.
    pub async fn run_on_node(
        &self,
        node: SshNodeConfig,
        command: impl Into<String>,
        use_sudo: bool,
    ) -> Result<NodeCommandResult, RemoteExecError> {
        self.run_on_node_async(node, command.into(), use_sudo).await
    }

    /// Run a command on all nodes in parallel and collect per-node output.
    pub async fn run_on_all(
        &self,
        nodes: Vec<SshNodeConfig>,
        command: impl Into<String>,
        use_sudo: bool,
    ) -> FanoutCommandResult {
        let command = command.into();
        let started_at = Utc::now();

        let mut handles = Vec::with_capacity(nodes.len());
        for node in nodes {
            let exec = self.clone();
            let command_clone = command.clone();
            handles.push(tokio::spawn(async move {
                match exec
                    .run_on_node(node.clone(), command_clone, use_sudo)
                    .await
                {
                    Ok(result) => result,
                    Err(err) => NodeCommandResult {
                        node: node.name,
                        host: node.host,
                        command: "<transport error>".to_string(),
                        started_at: Utc::now(),
                        duration_ms: 0,
                        success: false,
                        exit_code: None,
                        stdout: String::new(),
                        stderr: err.to_string(),
                    },
                }
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            if let Ok(result) = handle.await {
                results.push(result);
            }
        }

        FanoutCommandResult {
            command,
            started_at,
            completed_at: Utc::now(),
            results,
        }
    }

    async fn run_on_node_async(
        &self,
        node: SshNodeConfig,
        command: String,
        use_sudo: bool,
    ) -> Result<NodeCommandResult, RemoteExecError> {
        let mut options = SshConnectionOptions::from_node(&node);
        options.batch_mode = self.batch_mode;
        options.command_timeout_secs = Some(self.command_timeout_secs);

        let final_command = execution_envelope(&command, use_sudo);

        let connection = SshConnection::new(options);
        let output = connection.execute_async(&final_command).await?;
        let reported_command = if use_sudo {
            format!("sudo -n -- {command}")
        } else {
            command
        };

        Ok(NodeCommandResult {
            node: node.name,
            host: node.host,
            command: reported_command,
            started_at: output.started_at,
            duration_ms: output.duration_ms,
            success: output.success,
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

fn shell_single_quote_escape(input: &str) -> String {
    input.replace('\'', "'\\''")
}

/// Wrap a remote payload in the same deterministic execution environment used
/// by every immediate SSH caller (MCP and CLI).
fn execution_envelope(command: &str, use_sudo: bool) -> String {
    let command = shell_single_quote_escape(command);
    let payload = format!("/bin/sh -c '{command}'");
    let payload = shell_single_quote_escape(&payload);
    let mut path_setup =
        "ff_path=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string();
    if !use_sudo {
        path_setup.push_str(
            "; if [ \"$(/usr/bin/uname -s 2>/dev/null)\" = Darwin ]; then \
             ff_path=\"$ff_path:/opt/homebrew/bin\"; fi; \
             if [ -n \"${HOME:-}\" ]; then \
             ff_path=\"$ff_path:$HOME/.local/bin:$HOME/.cargo/bin\"; fi",
        );
    }
    let execute = format!("exec /usr/bin/env \"PATH=$ff_path\" /bin/sh -c '{payload}'");
    let body = if use_sudo {
        format!(
            "{path_setup}; if [ \"$(/usr/bin/id -u)\" -eq 0 ]; then {execute}; \
             else exec /usr/bin/sudo -n -- /usr/bin/env \"PATH=$ff_path\" /bin/sh -c '{payload}'; fi"
        )
    } else {
        format!("{path_setup}; {execute}")
    };
    format!("/bin/sh -c '{}'", shell_single_quote_escape(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_has_deterministic_cross_platform_path() {
        let envelope = execution_envelope("ff --version", false);
        for required in [
            "/usr/local/sbin",
            "/usr/local/bin",
            "/usr/sbin",
            "/usr/bin",
            "/sbin",
            "/bin",
            "$HOME/.local/bin",
            "$HOME/.cargo/bin",
            "/opt/homebrew/bin",
        ] {
            assert!(
                envelope.contains(required),
                "missing {required}: {envelope}"
            );
        }
        assert!(envelope.contains("uname -s"));
    }

    #[test]
    fn sudo_envelope_excludes_user_writable_path_entries() {
        let envelope = execution_envelope("true", true);
        assert!(!envelope.contains("$HOME/.local/bin"));
        assert!(!envelope.contains("$HOME/.cargo/bin"));
        assert!(!envelope.contains("/opt/homebrew/bin"));
    }

    #[test]
    fn sudo_envelope_is_immune_to_homebrew_symlink_and_parent_replacement() {
        let envelope = execution_envelope("true", true);
        // Root never consults the optional Homebrew tree, so symlinked bins,
        // writable/non-root ancestors, and path replacement races cannot affect
        // executable resolution.
        for attacker_controlled_component in ["/opt", "/opt/homebrew", "/opt/homebrew/bin"] {
            assert!(!envelope.contains(attacker_controlled_component));
        }
    }

    #[test]
    fn envelope_preserves_quotes_and_shell_syntax() {
        let envelope = execution_envelope("printf '%s\\n' \"a b\"; echo '$HOME'", false);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&envelope)
            .output()
            .expect("run envelope locally");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "a b\n$HOME\n");
    }

    #[test]
    fn sudo_envelope_is_root_safe_and_non_interactive() {
        let envelope = execution_envelope("id -u", true);
        assert!(envelope.contains("/usr/bin/id -u"));
        assert!(envelope.contains("/usr/bin/sudo -n --"));
        assert!(envelope.contains("if ["));
        assert!(
            std::process::Command::new("sh")
                .arg("-n")
                .arg("-c")
                .arg(envelope)
                .status()
                .expect("parse sudo envelope")
                .success()
        );
    }

    #[test]
    fn envelope_preserves_payload_exit_255() {
        let envelope = execution_envelope("exit 255", false);
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&envelope)
            .status()
            .expect("run envelope locally");
        assert_eq!(status.code(), Some(255));
    }

    #[test]
    fn zero_timeout_is_still_bounded() {
        assert_eq!(RemoteExecutor::new(0, true).command_timeout_secs, 1);
    }
}
