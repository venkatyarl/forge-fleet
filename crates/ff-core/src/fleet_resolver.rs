//! Centralized fleet node resolver.
//!
//! Provides a single source of truth for discovering fleet nodes across
//! every consumer — Rust daemons, CLI tools, and shell scripts.
//!
//! Resolution chain (in priority order):
//!   1. Postgres `fleet_nodes` table  — canonical source of truth
//!   2. `fleet.toml` `[nodes.*]`       — static config fallback
//!   3. `~/.ssh/config`                — SSH-based discovery
//!   4. `~/.forgefleet/fleet.json`    — JSON fallback
//!
//! Both sync (`resolve_sync`) and async (`resolve`) entry points are provided.
//! The sync path never spawns an async runtime; it reads files only.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::config::{FleetConfig, NodeConfig};

// ─── Public data type ────────────────────────────────────────────────────────

/// Lightweight fleet node descriptor returned by the resolver.
///
/// This is intentionally smaller than `ff_db::FleetNodeRow` — it carries only
/// the fields needed for SSH connectivity, deployment, and routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FleetNodeInfo {
    pub name: String,
    pub ip: String,
    pub ssh_user: String,
    pub os: String,
    pub role: String,
}

impl FleetNodeInfo {
    /// Whether this node runs a Unix-like OS (Linux or macOS).
    pub fn is_unix(&self) -> bool {
        let lower = self.os.to_ascii_lowercase();
        lower.contains("linux") || lower.contains("macos") || lower.contains("darwin")
    }

    /// Whether this node runs Linux specifically.
    pub fn is_linux(&self) -> bool {
        self.os.to_ascii_lowercase().contains("linux")
    }

    /// Whether this node runs macOS.
    pub fn is_macos(&self) -> bool {
        let lower = self.os.to_ascii_lowercase();
        lower.contains("macos") || lower.contains("darwin")
    }
}

// ─── Resolver ────────────────────────────────────────────────────────────────

/// Centralized fleet node resolver.
///
/// Construct with `FleetResolver::new()` then call `resolve()` (async) or
/// `resolve_sync()` (sync, file-based only).
#[derive(Debug, Clone)]
pub struct FleetResolver {
    config_path: PathBuf,
}

impl Default for FleetResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetResolver {
    /// Create a resolver that reads `~/.forgefleet/fleet.toml`.
    pub fn new() -> Self {
        let config_path = Self::default_config_path();
        Self { config_path }
    }

    /// Create a resolver with an explicit config path.
    pub fn with_config_path<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            config_path: path.into(),
        }
    }

    // ── Async entry point ───────────────────────────────────────────────────

    /// Resolve fleet nodes using the full chain.
    ///
    /// 1. Try Postgres via the DB URL in `fleet.toml`.
    /// 2. Fall back to `fleet.toml` `[nodes.*]`.
    /// 3. Fall back to `~/.ssh/config`.
    /// 4. Fall back to `~/.forgefleet/fleet.json`.
    pub async fn resolve(&self) -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
        // 1. Postgres
        match self.resolve_from_postgres().await {
            Ok(nodes) if !nodes.is_empty() => {
                debug!(count = nodes.len(), "resolved fleet nodes from Postgres");
                return Ok(nodes);
            }
            Ok(_) => debug!("Postgres returned empty node list"),
            Err(e) => debug!(error = %e, "Postgres resolution failed"),
        }

        // 2-4. File-based fallbacks
        self.resolve_sync()
    }

    // ── Sync entry point ────────────────────────────────────────────────────

    /// Resolve fleet nodes from files only (no Postgres, no async runtime).
    ///
    /// Chain:
    ///   1. `fleet.toml` `[nodes.*]`
    ///   2. `~/.ssh/config`
    ///   3. `~/.forgefleet/fleet.json`
    pub fn resolve_sync(&self) -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
        // 1. fleet.toml
        match self.resolve_from_fleet_toml() {
            Ok(nodes) if !nodes.is_empty() => {
                debug!(count = nodes.len(), "resolved fleet nodes from fleet.toml");
                return Ok(nodes);
            }
            Ok(_) => debug!("fleet.toml has no nodes"),
            Err(e) => debug!(error = %e, "fleet.toml resolution failed"),
        }

        // 2. ~/.ssh/config
        match self.resolve_from_ssh_config() {
            Ok(nodes) if !nodes.is_empty() => {
                debug!(count = nodes.len(), "resolved fleet nodes from ~/.ssh/config");
                return Ok(nodes);
            }
            Ok(_) => debug!("~/.ssh/config has no fleet nodes"),
            Err(e) => debug!(error = %e, "~/.ssh/config resolution failed"),
        }

        // 3. ~/.forgefleet/fleet.json
        match self.resolve_from_fleet_json() {
            Ok(nodes) if !nodes.is_empty() => {
                debug!(count = nodes.len(), "resolved fleet nodes from fleet.json");
                return Ok(nodes);
            }
            Ok(_) => debug!("fleet.json has no nodes"),
            Err(e) => debug!(error = %e, "fleet.json resolution failed"),
        }

        Err(FleetResolveError::NoSources)
    }

    // ── Individual source resolvers ─────────────────────────────────────────

    async fn resolve_from_postgres(&self) -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
        let config = self.load_fleet_config()?;
        let db_url = &config.database.url;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(db_url)
            .await
            .map_err(|e| FleetResolveError::Postgres(e.to_string()))?;

        #[derive(sqlx::FromRow)]
        struct NodeRow {
            name: String,
            ip: String,
            ssh_user: String,
            os: String,
            role: String,
        }

        let rows = sqlx::query_as::<_, NodeRow>(
            "SELECT name, ip, ssh_user, os, role FROM fleet_nodes ORDER BY election_priority, name",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| FleetResolveError::Postgres(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| FleetNodeInfo {
                name: r.name,
                ip: r.ip,
                ssh_user: r.ssh_user,
                os: r.os,
                role: r.role,
            })
            .collect())
    }

    fn resolve_from_fleet_toml(&self) -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
        let config = self.load_fleet_config()?;
        let nodes: Vec<FleetNodeInfo> = config
            .nodes
            .iter()
            .map(|(name, cfg)| node_config_to_info(name, cfg))
            .collect();
        Ok(nodes)
    }

    fn resolve_from_ssh_config(&self) -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
        let path = Self::home_dir()
            .ok_or(FleetResolveError::NoHomeDir)?
            .join(".ssh")
            .join("config");

        if !path.exists() {
            return Ok(Vec::new());
        }

        let content =
            std::fs::read_to_string(&path).map_err(|e| FleetResolveError::Io(e.to_string()))?;

        let mut nodes = Vec::new();
        let mut current_host: Option<String> = None;
        let mut current_hostname: Option<String> = None;
        let mut current_user = "venkat".to_string();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Host line starts a new block
            if let Some(rest) = trimmed.strip_prefix("Host ") {
                // Flush previous block
                if let (Some(host), Some(hostname)) = (current_host.take(), current_hostname.take())
                {
                    if !host.contains('*') && !host.to_ascii_lowercase().starts_with("github") {
                        nodes.push(FleetNodeInfo {
                            name: host.clone(),
                            ip: hostname,
                            ssh_user: current_user.clone(),
                            os: String::new(),
                            role: String::new(),
                        });
                    }
                }
                current_host = Some(rest.trim().to_string());
                current_hostname = None;
                current_user = "venkat".to_string();
                continue;
            }

            // HostName within a block
            if let Some(rest) = trimmed.strip_prefix("HostName ") {
                current_hostname = Some(rest.trim().to_string());
                continue;
            }

            // User within a block
            if let Some(rest) = trimmed.strip_prefix("User ") {
                current_user = rest.trim().to_string();
                continue;
            }
        }

        // Flush final block
        if let (Some(host), Some(hostname)) = (current_host, current_hostname) {
            if !host.contains('*') && !host.to_ascii_lowercase().starts_with("github") {
                nodes.push(FleetNodeInfo {
                    name: host,
                    ip: hostname,
                    ssh_user: current_user,
                    os: String::new(),
                    role: String::new(),
                });
            }
        }

        Ok(nodes)
    }

    fn resolve_from_fleet_json(&self) -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
        let path = Self::home_dir()
            .ok_or(FleetResolveError::NoHomeDir)?
            .join(".forgefleet")
            .join("fleet.json");

        if !path.exists() {
            return Ok(Vec::new());
        }

        let content =
            std::fs::read_to_string(&path).map_err(|e| FleetResolveError::Io(e.to_string()))?;

        #[derive(Deserialize)]
        struct FleetJson {
            #[serde(default)]
            nodes: Vec<FleetJsonNode>,
        }

        #[derive(Deserialize)]
        struct FleetJsonNode {
            name: String,
            ip: String,
            #[serde(default)]
            ssh_user: String,
            #[serde(default)]
            os: String,
            #[serde(default)]
            role: String,
        }

        let parsed: FleetJson =
            serde_json::from_str(&content).map_err(|e| FleetResolveError::Json(e.to_string()))?;

        Ok(parsed
            .nodes
            .into_iter()
            .map(|n| FleetNodeInfo {
                name: n.name,
                ip: n.ip,
                ssh_user: if n.ssh_user.is_empty() {
                    "venkat".to_string()
                } else {
                    n.ssh_user
                },
                os: n.os,
                role: n.role,
            })
            .collect())
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn load_fleet_config(&self) -> Result<FleetConfig, FleetResolveError> {
        if !self.config_path.exists() {
            return Err(FleetResolveError::ConfigNotFound(
                self.config_path.display().to_string(),
            ));
        }
        let content = std::fs::read_to_string(&self.config_path)
            .map_err(|e| FleetResolveError::Io(e.to_string()))?;
        toml::from_str(&content).map_err(|e| FleetResolveError::Toml(e.to_string()))
    }

    fn default_config_path() -> PathBuf {
        std::env::var("FORGEFLEET_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| Self::home_dir().map(|h| h.join(".forgefleet")))
            .unwrap_or_else(|| PathBuf::from("/tmp/.forgefleet"))
            .join("fleet.toml")
    }

    fn home_dir() -> Option<PathBuf> {
        std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
    }
}

fn node_config_to_info(name: &str, cfg: &NodeConfig) -> FleetNodeInfo {
    FleetNodeInfo {
        name: name.to_string(),
        ip: cfg.ip.clone(),
        ssh_user: cfg.ssh_user.clone().unwrap_or_else(|| "venkat".to_string()),
        os: cfg.os.clone().unwrap_or_default(),
        role: cfg.role.to_string(),
    }
}

// ─── Error type ──────────────────────────────────────────────────────────────

/// Errors that can occur during fleet node resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum FleetResolveError {
    ConfigNotFound(String),
    NoHomeDir,
    NoSources,
    Io(String),
    Toml(String),
    Json(String),
    Postgres(String),
}

impl std::fmt::Display for FleetResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigNotFound(p) => write!(f, "config not found: {p}"),
            Self::NoHomeDir => write!(f, "could not determine home directory"),
            Self::NoSources => write!(
                f,
                "no fleet node sources available (tried Postgres, fleet.toml, ~/.ssh/config, fleet.json)"
            ),
            Self::Io(s) => write!(f, "io error: {s}"),
            Self::Toml(s) => write!(f, "toml parse error: {s}"),
            Self::Json(s) => write!(f, "json parse error: {s}"),
            Self::Postgres(s) => write!(f, "postgres error: {s}"),
        }
    }
}

impl std::error::Error for FleetResolveError {}

// ─── Convenience free functions ──────────────────────────────────────────────

/// One-shot async resolution using the default resolver.
pub async fn resolve_fleet_nodes() -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
    FleetResolver::new().resolve().await
}

/// One-shot sync resolution using the default resolver (files only).
pub fn resolve_fleet_nodes_sync() -> Result<Vec<FleetNodeInfo>, FleetResolveError> {
    FleetResolver::new().resolve_sync()
}
