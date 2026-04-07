use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::SshNodeConfig;

/// SSH authentication mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SshAuth {
    /// Use SSH agent / default OpenSSH auth behavior.
    Agent,
    /// Use explicit private key file.
    KeyFile(PathBuf),
    /// Use password-based SSH auth (requires `sshpass`).
    Password(String),
}

/// Connection and invocation options for an SSH session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConnectionOptions {
    pub host: String,
    pub username: String,
    pub port: u16,
    pub auth: SshAuth,
    #[serde(default = "default_batch_mode")]
    pub batch_mode: bool,
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub command_timeout_secs: Option<u64>,
    #[serde(default = "default_strict_host_key_checking")]
    pub strict_host_key_checking: bool,
    #[serde(default)]
    pub known_hosts_path: Option<PathBuf>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_batch_mode() -> bool {
    true
}

fn default_strict_host_key_checking() -> bool {
    true
}

impl SshConnectionOptions {
    pub fn from_node(node: &SshNodeConfig) -> Self {
        let auth = match (node.key_path.clone(), node.password.clone()) {
            (Some(path), _) => SshAuth::KeyFile(path),
            (None, Some(password)) => SshAuth::Password(password),
            (None, None) => SshAuth::Agent,
        };

        Self {
            host: node.host.clone(),
            username: node.username.clone(),
            port: node.port,
            auth,
            batch_mode: node.batch_mode,
            connect_timeout_secs: node.connect_timeout_secs,
            command_timeout_secs: node.connect_timeout_secs,
            strict_host_key_checking: true,
            known_hosts_path: node.known_hosts_path.clone(),
            extra_args: Vec::new(),
        }
    }
}

/// Output captured from an SSH command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshCommandOutput {
    pub target: String,
    pub command: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u128,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum SshConnectionError {
    #[error("failed to spawn SSH command: {0}")]
    Spawn(#[from] std::io::Error),

    #[error("ssh command timed out after {timeout_secs}s")]
    TimedOut { timeout_secs: u64 },

    #[error("password auth requested but `sshpass` was not found in PATH")]
    MissingSshPass,
}

/// A lightweight SSH connection wrapper using OpenSSH binary calls.
#[derive(Debug, Clone)]
pub struct SshConnection {
    options: SshConnectionOptions,
}

impl SshConnection {
    pub fn new(options: SshConnectionOptions) -> Self {
        Self { options }
    }

    pub fn options(&self) -> &SshConnectionOptions {
        &self.options
    }

    /// Verify remote reachability/auth by running `echo connected`.
    pub fn connect(&self) -> Result<SshCommandOutput, SshConnectionError> {
        self.execute("echo connected")
    }

    /// Execute a remote command over SSH and capture stdout/stderr/exit code.
    pub fn execute(&self, remote_command: &str) -> Result<SshCommandOutput, SshConnectionError> {
        let started_at = Utc::now();
        let started = Instant::now();
        let mut cmd = self.build_command(remote_command)?;
        let output = self.run_command(&mut cmd)?;

        Ok(SshCommandOutput {
            target: format!(
                "{}@{}:{}",
                self.options.username, self.options.host, self.options.port
            ),
            command: remote_command.to_string(),
            started_at,
            duration_ms: started.elapsed().as_millis(),
            exit_code: output.status.code(),
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }

    fn build_command(&self, remote_command: &str) -> Result<Command, SshConnectionError> {
        let mut cmd = match &self.options.auth {
            SshAuth::Password(password) => {
                if !command_exists("sshpass") {
                    return Err(SshConnectionError::MissingSshPass);
                }
                let mut c = Command::new("sshpass");
                c.arg("-p").arg(password).arg("ssh");
                c
            }
            _ => Command::new("ssh"),
        };

        cmd.arg("-p").arg(self.options.port.to_string());

        cmd.arg("-o")
            .arg(format!("BatchMode={}", yes_no(self.options.batch_mode)));

        if let Some(timeout_secs) = self.options.connect_timeout_secs {
            cmd.arg("-o").arg(format!("ConnectTimeout={timeout_secs}"));
        }

        cmd.arg("-o").arg(format!(
            "StrictHostKeyChecking={}",
            yes_no(self.options.strict_host_key_checking)
        ));

        if let Some(path) = &self.options.known_hosts_path {
            cmd.arg("-o")
                .arg(format!("UserKnownHostsFile={}", path.display()));
        }

        if let SshAuth::KeyFile(path) = &self.options.auth {
            cmd.arg("-i").arg(path);
        }

        for arg in &self.options.extra_args {
            cmd.arg(arg);
        }

        cmd.arg(format!("{}@{}", self.options.username, self.options.host));
        cmd.arg(remote_command);

        Ok(cmd)
    }

    fn run_command(&self, cmd: &mut Command) -> Result<Output, SshConnectionError> {
        match self.options.command_timeout_secs {
            Some(timeout_secs) if timeout_secs > 0 => {
                run_with_timeout(cmd, Duration::from_secs(timeout_secs))
            }
            _ => cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(SshConnectionError::Spawn),
        }
    }
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, SshConnectionError> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(SshConnectionError::Spawn);
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SshConnectionError::TimedOut {
                timeout_secs: timeout.as_secs(),
            });
        }

        std::thread::sleep(Duration::from_millis(25));
    }
}

fn command_exists(binary: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {binary} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn yes_no(enabled: bool) -> &'static str {
    if enabled { "yes" } else { "no" }
}
