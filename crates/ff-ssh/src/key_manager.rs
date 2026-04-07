use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::config::SshNodeConfig;
use crate::connection::{SshConnection, SshConnectionError, SshConnectionOptions};

#[derive(Debug, Clone)]
pub struct KeyPair {
    pub private_key_path: PathBuf,
    pub public_key_path: PathBuf,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Error)]
pub enum KeyManagerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ssh error: {0}")]
    Ssh(#[from] SshConnectionError),

    #[error("command failed: {message}")]
    CommandFailed { message: String },

    #[error("key already exists: {path}")]
    KeyAlreadyExists { path: String },
}

/// SSH key utilities: key generation, authorization checks, known_hosts management.
#[derive(Debug, Clone)]
pub struct SshKeyManager {
    ssh_dir: PathBuf,
    known_hosts_path: PathBuf,
}

impl Default for SshKeyManager {
    fn default() -> Self {
        Self::new(default_ssh_dir())
    }
}

impl SshKeyManager {
    pub fn new(ssh_dir: PathBuf) -> Self {
        Self {
            known_hosts_path: ssh_dir.join("known_hosts"),
            ssh_dir,
        }
    }

    pub fn ssh_dir(&self) -> &Path {
        &self.ssh_dir
    }

    pub fn known_hosts_path(&self) -> &Path {
        &self.known_hosts_path
    }

    /// Generate an ed25519 key pair under `~/.ssh/<name>`.
    pub fn generate_key_pair(
        &self,
        name: &str,
        comment: Option<&str>,
        overwrite: bool,
    ) -> Result<KeyPair, KeyManagerError> {
        std::fs::create_dir_all(&self.ssh_dir)?;

        let private_key_path = self.ssh_dir.join(name);
        let public_key_path = self.ssh_dir.join(format!("{name}.pub"));

        if private_key_path.exists() && !overwrite {
            return Err(KeyManagerError::KeyAlreadyExists {
                path: private_key_path.display().to_string(),
            });
        }

        let mut cmd = Command::new("ssh-keygen");
        cmd.arg("-t")
            .arg("ed25519")
            .arg("-N")
            .arg("")
            .arg("-f")
            .arg(&private_key_path)
            .arg("-C")
            .arg(comment.unwrap_or("forgefleet"));

        let output = cmd.output()?;
        if !output.status.success() {
            return Err(KeyManagerError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let fingerprint = self.read_fingerprint(&public_key_path).ok();

        Ok(KeyPair {
            private_key_path,
            public_key_path,
            fingerprint,
        })
    }

    /// Ensure the given public key exists in remote `authorized_keys`.
    pub fn distribute_public_key(
        &self,
        node: &SshNodeConfig,
        public_key_path: &Path,
    ) -> Result<(), KeyManagerError> {
        let key = std::fs::read_to_string(public_key_path)?;
        let escaped = shell_single_quote_escape(key.trim());

        let command = format!(
            "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && (grep -qxF '{escaped}' ~/.ssh/authorized_keys || echo '{escaped}' >> ~/.ssh/authorized_keys)"
        );

        let connection = SshConnection::new(SshConnectionOptions::from_node(node));
        let result = connection.execute(&command)?;

        if !result.success {
            return Err(KeyManagerError::CommandFailed {
                message: format!(
                    "failed to install public key on {}: {}",
                    node.name, result.stderr
                ),
            });
        }

        Ok(())
    }

    /// Check whether the provided public key is already authorized on a node.
    pub fn is_key_authorized(
        &self,
        node: &SshNodeConfig,
        public_key: &str,
    ) -> Result<bool, KeyManagerError> {
        let escaped = shell_single_quote_escape(public_key.trim());
        let command = format!(
            "test -f ~/.ssh/authorized_keys && grep -qxF '{escaped}' ~/.ssh/authorized_keys"
        );

        let connection = SshConnection::new(SshConnectionOptions::from_node(node));
        let output = connection.execute(&command)?;
        Ok(output.success)
    }

    /// Add host key entry to known_hosts using `ssh-keyscan`.
    pub fn add_known_host(&self, host: &str, port: u16) -> Result<(), KeyManagerError> {
        std::fs::create_dir_all(&self.ssh_dir)?;

        let output = Command::new("ssh-keyscan")
            .arg("-p")
            .arg(port.to_string())
            .arg(host)
            .output()?;

        if !output.status.success() {
            return Err(KeyManagerError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let scan = String::from_utf8_lossy(&output.stdout).to_string();
        let existing = std::fs::read_to_string(&self.known_hosts_path).unwrap_or_default();

        let mut to_append = String::new();
        for line in scan.lines() {
            if !line.trim().is_empty() && !existing.contains(line) {
                to_append.push_str(line);
                to_append.push('\n');
            }
        }

        if !to_append.is_empty() {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.known_hosts_path)?;
            file.write_all(to_append.as_bytes())?;
        }

        Ok(())
    }

    /// Remove host entries from known_hosts.
    pub fn remove_known_host(&self, host: &str) -> Result<(), KeyManagerError> {
        if !self.known_hosts_path.exists() {
            return Ok(());
        }

        let output = Command::new("ssh-keygen")
            .arg("-R")
            .arg(host)
            .arg("-f")
            .arg(&self.known_hosts_path)
            .output()?;

        if !output.status.success() {
            return Err(KeyManagerError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    fn read_fingerprint(&self, public_key_path: &Path) -> Result<String, KeyManagerError> {
        let output = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(public_key_path)
            .output()?;

        if !output.status.success() {
            return Err(KeyManagerError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

fn default_ssh_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".ssh"))
        .unwrap_or_else(|_| PathBuf::from(".ssh"))
}

fn shell_single_quote_escape(input: &str) -> String {
    input.replace('\'', "'\\''")
}
