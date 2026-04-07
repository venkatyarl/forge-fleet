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
            command_timeout_secs,
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
        let command = command.into();
        let exec = self.clone();

        tokio::task::spawn_blocking(move || exec.run_on_node_blocking(node, command, use_sudo))
            .await?
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

    fn run_on_node_blocking(
        &self,
        node: SshNodeConfig,
        command: String,
        use_sudo: bool,
    ) -> Result<NodeCommandResult, RemoteExecError> {
        let mut options = SshConnectionOptions::from_node(&node);
        options.batch_mode = self.batch_mode;
        options.command_timeout_secs = Some(self.command_timeout_secs);

        let final_command = if use_sudo {
            format!("sudo -n sh -c '{}'", shell_single_quote_escape(&command))
        } else {
            command.clone()
        };

        let connection = SshConnection::new(options);
        let output = connection.execute(&final_command)?;

        Ok(NodeCommandResult {
            node: node.name,
            host: node.host,
            command: final_command,
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
