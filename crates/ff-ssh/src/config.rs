use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Per-node SSH settings derived from `fleet.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshNodeConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub alternate_ips: Vec<String>,
    #[serde(default = "default_batch_mode")]
    pub batch_mode: bool,
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub known_hosts_path: Option<PathBuf>,
}

impl SshNodeConfig {
    /// Build a config from ff-core node config with SSH-safe defaults.
    pub fn from_core_node(name: &str, node: &ff_core::config::NodeConfig) -> Self {
        Self {
            name: name.to_string(),
            host: node.ip.clone(),
            port: default_ssh_port(),
            username: node.ssh_user.clone().unwrap_or_else(default_username),
            key_path: None,
            password: None,
            alternate_ips: node.alt_ips.clone(),
            batch_mode: default_batch_mode(),
            connect_timeout_secs: Some(10),
            known_hosts_path: None,
        }
    }

    /// All candidate hosts for this node (primary host + alternates).
    pub fn candidate_hosts(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.host.as_str()).chain(self.alternate_ips.iter().map(String::as_str))
    }
}

/// Fleet-wide SSH config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FleetSshConfig {
    #[serde(default)]
    pub nodes: Vec<SshNodeConfig>,
}

impl FleetSshConfig {
    pub fn get_node(&self, name: &str) -> Option<&SshNodeConfig> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.iter().map(|n| n.name.as_str()).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct FleetTomlRaw {
    #[serde(default)]
    nodes: HashMap<String, RawNodeSshFields>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawNodeSshFields {
    #[serde(default)]
    username: Option<String>,
    #[serde(default, alias = "user", alias = "ssh_user", alias = "ssh_username")]
    ssh_username: Option<String>,
    #[serde(default, alias = "ssh_port")]
    port: Option<u16>,
    #[serde(default, alias = "ssh_key_path", alias = "identity_file")]
    key_path: Option<PathBuf>,
    #[serde(default, alias = "ssh_password")]
    password: Option<String>,
    #[serde(default, alias = "alt_ips", alias = "alternate_hosts")]
    alternate_ips: Vec<String>,
    #[serde(default)]
    batch_mode: Option<bool>,
    #[serde(default)]
    connect_timeout_secs: Option<u64>,
    #[serde(default)]
    known_hosts_path: Option<PathBuf>,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_username() -> String {
    std::env::var("FORGEFLEET_SSH_USER")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

fn default_batch_mode() -> bool {
    true
}

/// Load per-node SSH config from `fleet.toml`.
///
/// This combines ff-core's typed node config (name/host/port) with optional
/// SSH-specific fields (username, key path, alternate IPs, etc.) from the
/// same TOML file.
pub fn load_fleet_ssh_config(path: impl AsRef<Path>) -> Result<FleetSshConfig> {
    let path = path.as_ref();

    let core_cfg = ff_core::config::load_config(path)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("failed to load base fleet config from {}", path.display()))?;

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let raw_cfg: FleetTomlRaw =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;

    let ssh_overrides: HashMap<String, RawNodeSshFields> = raw_cfg.nodes;

    let nodes = core_cfg
        .nodes
        .iter()
        .map(|(name, node)| {
            let mut merged = SshNodeConfig::from_core_node(name, node);

            if let Some(extra) = ssh_overrides.get(name) {
                if let Some(username) = extra.ssh_username.as_ref().or(extra.username.as_ref()) {
                    merged.username = username.clone();
                }
                if let Some(port) = extra.port {
                    merged.port = port;
                }
                if extra.key_path.is_some() {
                    merged.key_path = extra.key_path.clone();
                }
                if extra.password.is_some() {
                    merged.password = extra.password.clone();
                }
                if !extra.alternate_ips.is_empty() {
                    merged.alternate_ips = extra.alternate_ips.clone();
                }
                if let Some(batch_mode) = extra.batch_mode {
                    merged.batch_mode = batch_mode;
                }
                if extra.connect_timeout_secs.is_some() {
                    merged.connect_timeout_secs = extra.connect_timeout_secs;
                }
                if extra.known_hosts_path.is_some() {
                    merged.known_hosts_path = extra.known_hosts_path.clone();
                }
            }

            merged
        })
        .collect();

    Ok(FleetSshConfig { nodes })
}
