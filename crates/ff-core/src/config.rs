//! Fleet configuration — load, parse, hot-reload, env overrides.
//!
//! The canonical config is `~/.forgefleet/fleet.toml`. This module provides:
//! - One-shot loading from disk
//! - Environment variable overrides (`FORGEFLEET_*`)
//! - Atomic in-memory config behind `Arc` + `watch` for hot-reload
//! - File-watcher that reloads on change (when tokio runtime is available)
//!
//! # Real fleet.toml structure
//!
//! ```toml
//! [general]
//! name = "ForgeFleet"
//! version = "1.0"
//!
//! [nodes.taylor]
//! ip = "192.168.5.100"
//! role = "gateway"
//!
//! [nodes.taylor.models.qwen35_35b]
//! name = "Qwen3.5-35B"
//! tier = 2
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::error::{ForgeFleetError, Result};
use crate::types::{Role, Runtime, Tier};

// ─── Config structs (mirror fleet.toml) ──────────────────────────────────────

/// Top-level fleet configuration.
///
/// Matches the production `~/.forgefleet/fleet.toml` format.
/// Uses `#[serde(default)]` liberally so missing sections don't break parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetConfig {
    /// Fleet-wide settings — `[general]` in fleet.toml.
    /// Also accepts `[fleet]` for backward compatibility with older configs/tests.
    #[serde(default, alias = "general")]
    pub fleet: FleetSettings,

    /// Named nodes — `[nodes.taylor]`, `[nodes.james]`, etc.
    /// Key is the node name, value is the node configuration.
    #[serde(default)]
    pub nodes: HashMap<String, NodeConfig>,

    /// Notification channels — `[notifications]`.
    #[serde(default)]
    pub notifications: NotificationsConfig,

    /// Runtime transport adapters — `[transport]`.
    #[serde(default)]
    pub transport: TransportConfig,

    /// External services — `[services.mc]`, `[services.hireflow_backend]`, etc.
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,

    /// LLM port/timeout configuration — `[llm]`.
    #[serde(default)]
    pub llm: LlmConfig,

    /// Global port assignments — `[ports]`.
    #[serde(default)]
    pub ports: PortsConfig,

    /// Scheduling configuration — `[scheduling]`.
    #[serde(default)]
    pub scheduling: SchedulingConfig,

    /// Embedded agent runtime settings — `[agent]`.
    #[serde(default)]
    pub agent: AgentSettings,

    /// Background operational loops (evolution, updater, self-heal, MCP federation).
    ///
    /// Config section: `[loops]` with nested `[loops.<name>]` tables.
    #[serde(default)]
    pub loops: LoopSettings,

    /// MCP service configuration — `[mcp.openclaw]`, `[mcp.forgefleet]`, etc.
    #[serde(default)]
    pub mcp: HashMap<String, McpConfig>,

    /// Enrollment/bootstrap configuration — `[enrollment]`.
    #[serde(default)]
    pub enrollment: EnrollmentConfig,

    /// Database connection — `[database]`.
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Redis connection for Fleet Pulse real-time metrics — `[redis]`.
    #[serde(default)]
    pub redis: RedisConfig,

    /// Pending nodes awaiting bootstrap — `[[bootstrap_targets]]`.
    #[serde(default)]
    pub bootstrap_targets: Vec<BootstrapTarget>,

    /// Leader election preferences — `[leader]`.
    /// Not present in all fleet.toml files but kept for election logic.
    #[serde(default)]
    pub leader: LeaderConfig,

    /// Standalone model definitions — backward compat for programmatic construction.
    /// Real fleet.toml nests models under `[nodes.<name>.models.<slug>]`.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

#[allow(clippy::derivable_impls)]
impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            fleet: FleetSettings::default(),
            nodes: HashMap::new(),
            notifications: NotificationsConfig::default(),
            transport: TransportConfig::default(),
            services: HashMap::new(),
            llm: LlmConfig::default(),
            ports: PortsConfig::default(),
            scheduling: SchedulingConfig::default(),
            agent: AgentSettings::default(),
            loops: LoopSettings::default(),
            mcp: HashMap::new(),
            enrollment: EnrollmentConfig::default(),
            database: DatabaseConfig::default(),
            redis: RedisConfig::default(),
            bootstrap_targets: vec![],
            leader: LeaderConfig::default(),
            models: vec![],
        }
    }
}

// ─── Convenience methods ─────────────────────────────────────────────────────

impl FleetConfig {
    /// Iterate over nodes as `(name, config)` pairs.
    pub fn nodes_iter(&self) -> impl Iterator<Item = (&str, &NodeConfig)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Get a node config by name.
    pub fn get_node(&self, name: &str) -> Option<&NodeConfig> {
        self.nodes.get(name)
    }

    /// Get all node-level model configs as `(node_name, model_slug, model_config)`.
    pub fn all_node_models(&self) -> Vec<(&str, &str, &NodeModelConfig)> {
        self.nodes
            .iter()
            .flat_map(|(node_name, node)| {
                node.models
                    .iter()
                    .map(move |(slug, model)| (node_name.as_str(), slug.as_str(), model))
            })
            .collect()
    }
}

// ── FleetSettings (maps to [general]) ────────────────────────────────────────

/// Fleet-wide settings from the `[general]` section (or `[fleet]` for backward compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSettings {
    /// Human-readable fleet name.
    #[serde(default = "default_fleet_name")]
    pub name: String,

    /// Config version string (e.g. "1.0").
    #[serde(default)]
    pub version: Option<String>,

    /// Default working repository path.
    #[serde(default)]
    pub default_repo: Option<String>,

    // ── Kept for backward compatibility / runtime use ──
    /// Heartbeat interval in seconds.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,

    /// Heartbeat timeout before marking a node offline.
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_secs: u64,

    /// Base API port.
    #[serde(default = "default_api_port")]
    pub api_port: u16,
}

impl Default for FleetSettings {
    fn default() -> Self {
        Self {
            name: default_fleet_name(),
            version: None,
            default_repo: None,
            heartbeat_interval_secs: default_heartbeat_interval(),
            heartbeat_timeout_secs: default_heartbeat_timeout(),
            api_port: default_api_port(),
        }
    }
}

/// Backward-compatible type alias.
pub type GeneralConfig = FleetSettings;

fn default_fleet_name() -> String {
    "ForgeFleet".into()
}
fn default_heartbeat_interval() -> u64 {
    15
}
fn default_heartbeat_timeout() -> u64 {
    45
}
fn default_api_port() -> u16 {
    51000
}

// ── NodeConfig (maps to [nodes.<name>]) ──────────────────────────────────────

/// Per-node configuration from `[nodes.<name>]` in fleet.toml.
///
/// The node name comes from the `HashMap` key, not a field in this struct.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeConfig {
    /// IP address (primary network identifier).
    /// Also accepts `host` for backward compatibility.
    #[serde(default, alias = "host")]
    pub ip: String,

    /// SSH user for remote access.
    #[serde(default)]
    pub ssh_user: Option<String>,

    /// RAM in GB (top-level shorthand — also available in `resources`).
    #[serde(default)]
    pub ram_gb: Option<u64>,

    /// CPU cores (top-level shorthand — also available in `resources`).
    #[serde(default)]
    pub cpu_cores: Option<u32>,

    /// Operating system (free-form string, e.g. "macOS 26.3", "Ubuntu 24.04").
    #[serde(default)]
    pub os: Option<String>,

    /// Node role — "gateway", "builder", "leader", "worker".
    #[serde(default)]
    pub role: Role,

    /// Alternative IP addresses for this node.
    #[serde(default)]
    pub alt_ips: Vec<String>,

    /// Models deployed on this node, keyed by model slug.
    /// Example: `[nodes.taylor.models.qwen35_35b]`
    #[serde(default)]
    pub models: HashMap<String, NodeModelConfig>,

    /// Detailed resource spec — `[nodes.<name>.resources]`.
    #[serde(default)]
    pub resources: Option<NodeResources>,

    /// Node capabilities — `[nodes.<name>.capabilities]`.
    #[serde(default)]
    pub capabilities: Option<NodeCapabilities>,

    /// Workload preferences — `[nodes.<name>.preferences]`.
    #[serde(default)]
    pub preferences: Option<NodePreferences>,

    // ── Backward-compat / election fields ──
    /// Election priority (lower = more preferred). Defaults to 100.
    #[serde(default)]
    pub election_priority: Option<u32>,

    /// API port override (backward compat).
    #[serde(default)]
    pub port: Option<u16>,
}

impl NodeConfig {
    /// Get the election priority, defaulting to 100 if not set.
    pub fn priority(&self) -> u32 {
        self.election_priority.unwrap_or(100)
    }

    /// Get the effective host/IP address.
    pub fn host(&self) -> &str {
        &self.ip
    }

    /// Get effective RAM in GB, checking top-level then resources.
    pub fn effective_ram_gb(&self) -> Option<u64> {
        self.ram_gb
            .or_else(|| self.resources.as_ref().and_then(|r| r.ram_gb))
    }

    /// Get effective CPU cores, checking top-level then resources.
    pub fn effective_cpu_cores(&self) -> Option<u32> {
        self.cpu_cores
            .or_else(|| self.resources.as_ref().and_then(|r| r.cpu_cores))
    }
}

// ── Node Model Config ────────────────────────────────────────────────────────

/// Model deployed on a specific node: `[nodes.<name>.models.<slug>]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeModelConfig {
    /// Human-readable model name (e.g. "Qwen3.5-35B").
    #[serde(default)]
    pub name: String,

    /// Model family (e.g. "qwen", "llama").
    #[serde(default)]
    pub family: Option<String>,

    /// Port this model listens on.
    #[serde(default)]
    pub port: Option<u16>,

    /// Tier as integer (1 = fast/9B, 2 = code/32B, 3 = review/72B, 4 = expert/200B+).
    #[serde(default = "default_tier")]
    pub tier: u32,

    /// Whether the model runs locally on this node.
    #[serde(default)]
    pub local: Option<bool>,

    /// Lifecycle stage ("production", "staging", "experimental").
    #[serde(default)]
    pub lifecycle: Option<String>,

    /// Run mode ("always_on", "on_demand").
    #[serde(default)]
    pub mode: Option<String>,

    /// Preferred workloads for this model.
    #[serde(default)]
    pub preferred_workloads: Vec<String>,
}

fn default_tier() -> u32 {
    1
}

// ── Node Resources ───────────────────────────────────────────────────────────

/// Detailed resource spec for a node: `[nodes.<name>.resources]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeResources {
    #[serde(default)]
    pub cpu_cores: Option<u32>,
    #[serde(default)]
    pub ram_gb: Option<u64>,
    #[serde(default)]
    pub vram_gb: Option<u64>,
}

// ── Node Capabilities ────────────────────────────────────────────────────────

/// Capability flags for a node: `[nodes.<name>.capabilities]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeCapabilities {
    #[serde(default)]
    pub control_plane: Option<bool>,
    #[serde(default)]
    pub model_building: Option<bool>,
    #[serde(default)]
    pub premium_inference: Option<bool>,
    #[serde(default)]
    pub local_inference: Option<bool>,
    #[serde(default)]
    pub docker: Option<bool>,
}

// ── Node Preferences ─────────────────────────────────────────────────────────

/// Workload preferences for a node: `[nodes.<name>.preferences]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodePreferences {
    #[serde(default)]
    pub preferred_workloads: Vec<String>,
    #[serde(default)]
    pub first_preference_workloads: Vec<String>,
}

// ── Notifications ────────────────────────────────────────────────────────────

/// Notification configuration — `[notifications]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotificationsConfig {
    /// Telegram notifications — `[notifications.telegram]`.
    #[serde(default)]
    pub telegram: Option<TelegramNotification>,
}

/// Telegram notification settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramNotification {
    /// Telegram chat ID to send notifications to.
    pub chat_id: String,
    /// Channel type (e.g. "telegram").
    #[serde(default)]
    pub channel: Option<String>,
}

/// Runtime transport configuration — `[transport]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransportConfig {
    /// Telegram bidirectional transport settings — `[transport.telegram]`.
    #[serde(default)]
    pub telegram: Option<TelegramTransportConfig>,
}

/// Telegram transport settings for polling-based two-way messaging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramTransportConfig {
    /// Enable Telegram polling transport.
    #[serde(default)]
    pub enabled: bool,

    /// Bot token for Telegram Bot API.
    ///
    /// If omitted, `bot_token_env` is consulted.
    #[serde(default)]
    pub bot_token: Option<String>,

    /// Environment variable name to read bot token from.
    #[serde(default = "default_telegram_bot_token_env")]
    pub bot_token_env: String,

    /// Whitelist of chat IDs allowed to control ForgeFleet.
    ///
    /// Empty list means allow all chats.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,

    /// Delay between polling loops in seconds.
    #[serde(default = "default_telegram_poll_interval")]
    pub polling_interval_secs: u64,

    /// Long-poll timeout passed to Telegram `getUpdates`.
    #[serde(default = "default_telegram_poll_timeout")]
    pub polling_timeout_secs: u64,

    /// Optional directory for downloaded media attachments.
    #[serde(default)]
    pub media_download_dir: Option<String>,

    /// Maximum allowed Telegram media attachment size (bytes) for ingest.
    #[serde(default = "default_telegram_media_max_file_size_bytes")]
    pub media_max_file_size_bytes: u64,

    /// Allowed MIME types for Telegram media ingest.
    ///
    /// Supports exact values (`image/jpeg`) and prefix wildcards (`image/*`).
    #[serde(default = "default_telegram_media_allowed_mime_types")]
    pub media_allowed_mime_types: Vec<String>,
}

impl Default for TelegramTransportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: None,
            bot_token_env: default_telegram_bot_token_env(),
            allowed_chat_ids: Vec::new(),
            polling_interval_secs: default_telegram_poll_interval(),
            polling_timeout_secs: default_telegram_poll_timeout(),
            media_download_dir: None,
            media_max_file_size_bytes: default_telegram_media_max_file_size_bytes(),
            media_allowed_mime_types: default_telegram_media_allowed_mime_types(),
        }
    }
}

impl TelegramTransportConfig {
    /// Resolve bot token from inline config or configured env var.
    pub fn resolve_bot_token(&self) -> Option<String> {
        if let Some(token) = self.bot_token.as_deref().map(str::trim)
            && !token.is_empty()
        {
            return Some(token.to_string());
        }

        let env_key = self.bot_token_env.trim();
        if env_key.is_empty() {
            return None;
        }

        std::env::var(env_key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Whether this chat is authorized.
    pub fn is_chat_allowed(&self, chat_id: i64) -> bool {
        self.allowed_chat_ids.is_empty() || self.allowed_chat_ids.contains(&chat_id)
    }
}

fn default_telegram_bot_token_env() -> String {
    "FORGEFLEET_TELEGRAM_BOT_TOKEN".to_string()
}

fn default_telegram_poll_interval() -> u64 {
    2
}

fn default_telegram_poll_timeout() -> u64 {
    15
}

fn default_telegram_media_max_file_size_bytes() -> u64 {
    25 * 1024 * 1024
}

fn default_telegram_media_allowed_mime_types() -> Vec<String> {
    vec![
        "image/*".to_string(),
        "video/*".to_string(),
        "application/octet-stream".to_string(),
    ]
}

// ── Services ─────────────────────────────────────────────────────────────────

/// External service definition — `[services.<name>]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Port the service listens on.
    pub port: u16,
}

// ── LLM ──────────────────────────────────────────────────────────────────────

/// LLM configuration — `[llm]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmConfig {
    /// Ports used for LLM inference endpoints.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Port for announcing model availability.
    #[serde(default)]
    pub announce_port: Option<u16>,
    /// Per-tier timeout configuration.
    #[serde(default)]
    pub timeouts: LlmTimeouts,
}

/// Per-tier timeout config — `[llm.timeouts]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmTimeouts {
    /// Tier 1 timeout in seconds.
    #[serde(default)]
    pub tier1: Option<u64>,
    /// Tier 2 timeout in seconds.
    #[serde(default)]
    pub tier2: Option<u64>,
    /// Tier 3 timeout in seconds.
    #[serde(default)]
    pub tier3: Option<u64>,
    /// Tier 4 timeout in seconds.
    #[serde(default)]
    pub tier4: Option<u64>,
}

// ── Ports ────────────────────────────────────────────────────────────────────

/// Global port assignments — `[ports]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PortsConfig {
    #[serde(default)]
    pub openclaw: Option<u16>,
    #[serde(default)]
    pub forgefleet: Option<u16>,
    #[serde(default)]
    pub model_start: Option<u16>,
    #[serde(default)]
    pub model_end: Option<u16>,
    #[serde(default)]
    pub docker_app_start: Option<u16>,
    #[serde(default)]
    pub docker_app_end: Option<u16>,
}

// ── Scheduling ───────────────────────────────────────────────────────────────

/// Task scheduling configuration — `[scheduling]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchedulingConfig {
    /// Node that is the canonical writer for config changes.
    #[serde(default)]
    pub canonical_writer: Option<String>,
    /// Whether to use degraded coordinator mode.
    #[serde(default)]
    pub degraded_coordinator_mode: Option<bool>,
    /// Whether to filter by node capabilities.
    #[serde(default)]
    pub use_capability_filtering: Option<bool>,
    /// Whether to use resource-aware routing.
    #[serde(default)]
    pub use_resource_aware_routing: Option<bool>,
    /// Whether to use preference scoring.
    #[serde(default)]
    pub use_preference_scoring: Option<bool>,
    /// Whether to use speed-to-completion estimates.
    #[serde(default)]
    pub use_speed_to_completion: Option<bool>,
    /// Maximum handoffs per task.
    #[serde(default)]
    pub max_handoffs_per_task: Option<u32>,
}

/// Embedded agent configuration.
///
/// Controls behavior of the embedded `ff-agent` subsystem when running inside
/// the root `forgefleetd` process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Enable autonomous execution mode (claim/decompose/execute/report).
    ///
    /// Defaults to `false` to preserve heartbeat-only compatibility.
    #[serde(default)]
    pub autonomous_mode: bool,

    /// Poll interval used by autonomous mode when claiming work.
    #[serde(default = "default_agent_poll_interval")]
    pub poll_interval_secs: u64,

    /// Optional ownership/lease API base URL.
    ///
    /// If set, autonomous claims will attempt lease acquisition via this API.
    #[serde(default)]
    pub ownership_api_base_url: Option<String>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            autonomous_mode: false,
            poll_interval_secs: default_agent_poll_interval(),
            ownership_api_base_url: None,
        }
    }
}

fn default_agent_poll_interval() -> u64 {
    8
}

// ── Operational loops ───────────────────────────────────────────────────────

/// Background loop configuration — `[loops]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoopSettings {
    /// Evolution/improvement loop settings — `[loops.evolution]`.
    #[serde(default)]
    pub evolution: EvolutionLoopSettings,

    /// Updater polling/application settings — `[loops.updater]`.
    #[serde(default)]
    pub updater: UpdaterLoopSettings,

    /// Runtime self-heal loop settings — `[loops.self_heal]`.
    #[serde(default)]
    pub self_heal: SelfHealLoopSettings,

    /// MCP federation discovery/topology loop settings — `[loops.mcp_federation]`.
    #[serde(default)]
    pub mcp_federation: McpFederationLoopSettings,
}

/// Evolution loop settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionLoopSettings {
    /// Enable periodic evolution runs from runtime observations.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Interval between evolution cycles.
    #[serde(default = "default_evolution_interval")]
    pub interval_secs: u64,

    /// Minimum improvement ratio used by verifier scoring.
    #[serde(default = "default_minimum_improvement_ratio")]
    pub minimum_improvement_ratio: f32,
}

impl Default for EvolutionLoopSettings {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            interval_secs: default_evolution_interval(),
            minimum_improvement_ratio: default_minimum_improvement_ratio(),
        }
    }
}

fn default_evolution_interval() -> u64 {
    120
}

fn default_minimum_improvement_ratio() -> f32 {
    0.2
}

/// Updater loop settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterLoopSettings {
    /// Enable periodic update checks.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Interval between git update checks.
    #[serde(default = "default_update_check_interval")]
    pub check_interval_secs: u64,

    /// Enable automatic update apply pipeline (build/verify/swap).
    ///
    /// Defaults to false for safety.
    #[serde(default)]
    pub auto_apply: bool,

    /// Optional explicit source repo path for updater check/build.
    #[serde(default)]
    pub repo_path: Option<String>,

    /// Optional explicit current binary path for swapper.
    #[serde(default)]
    pub current_binary_path: Option<String>,

    /// Git remote to track for updates.
    #[serde(default = "default_updater_remote")]
    pub git_remote: String,

    /// Git branch to track for updates.
    #[serde(default = "default_updater_branch")]
    pub git_branch: String,
}

impl Default for UpdaterLoopSettings {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            check_interval_secs: default_update_check_interval(),
            auto_apply: false,
            repo_path: None,
            current_binary_path: None,
            git_remote: default_updater_remote(),
            git_branch: default_updater_branch(),
        }
    }
}

fn default_update_check_interval() -> u64 {
    3600
}

fn default_updater_remote() -> String {
    "origin".to_string()
}

fn default_updater_branch() -> String {
    "main".to_string()
}

/// Runtime self-heal loop settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfHealLoopSettings {
    /// Enable process self-heal automation.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Loop interval.
    #[serde(default = "default_self_heal_interval")]
    pub interval_secs: u64,

    /// Adopt externally-started llama-server processes on expected ports.
    #[serde(default = "default_true")]
    pub auto_adopt: bool,

    /// Restart threshold after consecutive health failures.
    #[serde(default = "default_self_heal_max_failures")]
    pub max_health_failures: u32,

    /// Health probe timeout in seconds.
    #[serde(default = "default_self_heal_probe_timeout")]
    pub health_probe_timeout_secs: u64,

    /// Graceful stop timeout in seconds.
    #[serde(default = "default_self_heal_stop_timeout")]
    pub stop_timeout_secs: u64,
}

impl Default for SelfHealLoopSettings {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            interval_secs: default_self_heal_interval(),
            auto_adopt: default_true(),
            max_health_failures: default_self_heal_max_failures(),
            health_probe_timeout_secs: default_self_heal_probe_timeout(),
            stop_timeout_secs: default_self_heal_stop_timeout(),
        }
    }
}

fn default_self_heal_interval() -> u64 {
    30
}

fn default_self_heal_max_failures() -> u32 {
    3
}

fn default_self_heal_probe_timeout() -> u64 {
    5
}

fn default_self_heal_stop_timeout() -> u64 {
    10
}

/// MCP federation/topology loop settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpFederationLoopSettings {
    /// Enable periodic MCP federation discovery and topology validation.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Loop interval.
    #[serde(default = "default_mcp_federation_interval")]
    pub interval_secs: u64,

    /// Default request timeout for remote MCP probes.
    #[serde(default = "default_mcp_request_timeout")]
    pub request_timeout_secs: u64,
}

impl Default for McpFederationLoopSettings {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            interval_secs: default_mcp_federation_interval(),
            request_timeout_secs: default_mcp_request_timeout(),
        }
    }
}

fn default_mcp_federation_interval() -> u64 {
    120
}

fn default_mcp_request_timeout() -> u64 {
    5
}

fn default_true() -> bool {
    true
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// MCP service config — `[mcp.<name>]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    /// Whether this MCP acts as a server.
    #[serde(default)]
    pub server: Option<bool>,
    /// Whether this MCP acts as a client.
    #[serde(default)]
    pub client: Option<bool>,
    /// Port for this MCP service.
    #[serde(default)]
    pub port: Option<u16>,

    /// Explicit MCP endpoint (for remote federation clients).
    /// Example: `"http://10.0.0.5:51821/mcp"`
    #[serde(default, alias = "url")]
    pub endpoint: Option<String>,

    /// Whether this MCP endpoint is required for healthy topology.
    #[serde(default)]
    pub required: Option<bool>,

    /// Tools that must exist on this MCP endpoint.
    #[serde(default)]
    pub required_tools: Vec<String>,

    /// Optional tools that are nice-to-have.
    #[serde(default)]
    pub optional_tools: Vec<String>,

    /// Required MCP service dependencies by name.
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Optional MCP service dependencies by name.
    #[serde(default)]
    pub optional_depends_on: Vec<String>,

    /// Optional per-service request timeout override (seconds).
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

// ── Enrollment ───────────────────────────────────────────────────────────────

/// Enrollment/bootstrap configuration — `[enrollment]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentConfig {
    /// Engine used for bootstrapping (e.g. "forgefleet").
    #[serde(default)]
    pub bootstrap_engine: Option<String>,
    /// Interface used for bootstrapping (e.g. "openclaw").
    #[serde(default)]
    pub bootstrap_interface: Option<String>,
    /// Whether SSH access is required before bootstrapping.
    #[serde(default)]
    pub require_ssh_before_bootstrap: Option<bool>,
    /// Whether to auto-enroll after healthcheck passes.
    #[serde(default)]
    pub auto_enroll_after_healthcheck: Option<bool>,
    /// Whether enrollment endpoints must validate a shared-secret token.
    ///
    /// `true`  (default) — fail-closed; enrollment requires a matching token.
    /// `false` — open mode; any request can enroll. Intended for trusted LANs
    ///           only. The gateway logs a WARN on every request when disabled.
    #[serde(default = "default_require_shared_secret")]
    pub require_shared_secret: bool,
    /// Shared secret/token required by `/api/fleet/enroll` when
    /// `require_shared_secret = true`. If omitted, the runtime resolves from
    /// `shared_secret_env`.
    #[serde(default)]
    pub shared_secret: Option<String>,
    /// Environment variable name used to resolve the enrollment secret.
    #[serde(default = "default_enrollment_secret_env")]
    pub shared_secret_env: String,
    /// Default role assigned when an enrolling node does not request one.
    #[serde(default)]
    pub default_role: Option<String>,
    /// Optional enrollment role allowlist.
    ///
    /// Empty list means any known role is allowed.
    #[serde(default)]
    pub allowed_roles: Vec<String>,
    /// Optional heartbeat interval override (seconds) returned to enrolled nodes.
    /// Falls back to `fleet.heartbeat_interval_secs` when omitted.
    #[serde(default)]
    pub heartbeat_interval_secs: Option<u64>,
}

impl Default for EnrollmentConfig {
    fn default() -> Self {
        Self {
            bootstrap_engine: None,
            bootstrap_interface: None,
            require_ssh_before_bootstrap: None,
            auto_enroll_after_healthcheck: None,
            require_shared_secret: default_require_shared_secret(),
            shared_secret: None,
            shared_secret_env: default_enrollment_secret_env(),
            default_role: None,
            allowed_roles: Vec::new(),
            heartbeat_interval_secs: None,
        }
    }
}

/// Outcome of consulting enrollment policy at a request site.
#[derive(Debug, Clone)]
pub enum EnrollmentEnforcement {
    /// `require_shared_secret=false`. Accept request without a token check.
    /// Callers MUST log a warning so open mode is never silent.
    Disabled,
    /// Token check enabled; compare presented token against this value.
    Required(String),
    /// Token check enabled but no secret is configured anywhere. Reject 503.
    MisconfiguredRequired,
}

impl EnrollmentConfig {
    /// Resolve enrollment shared secret from config or environment.
    pub fn resolve_shared_secret(&self) -> Option<String> {
        if let Some(secret) = self.shared_secret.as_ref().map(|value| value.trim())
            && !secret.is_empty()
        {
            return Some(secret.to_string());
        }

        let env_key = self.shared_secret_env.trim();
        if env_key.is_empty() {
            return None;
        }

        std::env::var(env_key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Decide what enforcement every enrollment endpoint should apply.
    pub fn enforcement_policy(&self) -> EnrollmentEnforcement {
        if !self.require_shared_secret {
            return EnrollmentEnforcement::Disabled;
        }
        match self.resolve_shared_secret() {
            Some(tok) => EnrollmentEnforcement::Required(tok),
            None => EnrollmentEnforcement::MisconfiguredRequired,
        }
    }
}

fn default_enrollment_secret_env() -> String {
    "FORGEFLEET_ENROLLMENT_TOKEN".to_string()
}

fn default_require_shared_secret() -> bool {
    true
}

// ── Database ─────────────────────────────────────────────────────────────────

/// Database mode selector for runtime persistence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseMode {
    /// Full embedded SQLite runtime (default).
    EmbeddedSqlite,
    /// Transitional mode: runtime registry + enrollment events in Postgres,
    /// while legacy tables remain in embedded SQLite.
    #[serde(alias = "postgres")]
    PostgresRuntime,
    /// Target end-state mode: all runtime persistence should be Postgres.
    ///
    /// Startup preflight guards enforce explicit cutover evidence and fail
    /// loudly when any SQLite-only dependency remains.
    #[serde(alias = "full_postgres", alias = "full-postgres")]
    PostgresFull,
}

impl Default for DatabaseMode {
    fn default() -> Self {
        Self::EmbeddedSqlite
    }
}

impl DatabaseMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EmbeddedSqlite => "embedded_sqlite",
            Self::PostgresRuntime => "postgres_runtime",
            Self::PostgresFull => "postgres_full",
        }
    }
}

/// Database configuration — `[database]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Runtime persistence mode.
    #[serde(default)]
    pub mode: DatabaseMode,

    /// Optional override for embedded SQLite file path.
    ///
    /// Relative paths resolve against the fleet.toml directory.
    #[serde(default)]
    pub sqlite_path: Option<String>,

    /// Database host (e.g. "127.0.0.1").
    #[serde(default)]
    pub host: Option<String>,

    /// Database port (e.g. 55432).
    #[serde(default)]
    pub port: Option<u16>,

    /// Database name (e.g. "forgefleet").
    #[serde(default)]
    pub name: Option<String>,

    /// Database user.
    #[serde(default)]
    pub user: Option<String>,

    /// Database password.
    #[serde(default)]
    pub password: Option<String>,

    /// Full Postgres connection URL.
    #[serde(default = "default_database_url")]
    pub url: String,

    /// Max Postgres connections for runtime registry pool.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Explicit evidence marker for SQLite→Postgres full cutover.
    ///
    /// Examples: ticket ID, signed runbook path, or artifact URL containing
    /// backup + validation proof. Required when `mode = "postgres_full"`.
    #[serde(default)]
    pub cutover_evidence: Option<String>,
}

impl DatabaseConfig {
    /// Whether runtime registry should be persisted in Postgres.
    pub fn uses_postgres_runtime(&self) -> bool {
        matches!(self.mode, DatabaseMode::PostgresRuntime)
    }

    /// Whether this mode expects full Postgres cutover semantics.
    pub fn requires_postgres_full_cutover(&self) -> bool {
        matches!(self.mode, DatabaseMode::PostgresFull)
    }

    /// Optional cutover evidence token/path required for `postgres_full`.
    pub fn cutover_evidence_ref(&self) -> Option<&str> {
        self.cutover_evidence
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            mode: DatabaseMode::default(),
            sqlite_path: None,
            host: None,
            port: None,
            name: None,
            user: None,
            password: None,
            url: default_database_url(),
            max_connections: default_max_connections(),
            cutover_evidence: None,
        }
    }
}

fn default_database_url() -> String {
    "postgres://forgefleet:forgefleet@localhost/forgefleet".into()
}
fn default_max_connections() -> u32 {
    10
}

// ── Redis Config (Fleet Pulse) ───────────────────────────────────────────────

/// Redis connection configuration for Fleet Pulse real-time metrics.
///
/// Config section: `[redis]` in fleet.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    /// Redis connection URL.
    #[serde(default = "default_redis_url")]
    pub url: String,

    /// Key prefix for all Fleet Pulse keys.
    #[serde(default = "default_redis_prefix")]
    pub prefix: String,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: default_redis_url(),
            prefix: default_redis_prefix(),
        }
    }
}

fn default_redis_url() -> String {
    "redis://127.0.0.1:6379".into()
}
fn default_redis_prefix() -> String {
    "pulse".into()
}

// ── Bootstrap Targets ────────────────────────────────────────────────────────

/// A pending node awaiting bootstrap — `[[bootstrap_targets]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapTarget {
    /// Node name.
    pub name: String,
    /// Bootstrap status ("in_progress", "received", "ordered", etc.).
    #[serde(default)]
    pub status: Option<String>,
    /// Operating system.
    #[serde(default)]
    pub os: Option<String>,
    /// Hardware description (e.g. "NVIDIA DGX Spark").
    #[serde(default)]
    pub hardware: Option<String>,
    /// Whether the node is reachable by SSH.
    #[serde(default)]
    pub reachable_by_ssh: Option<bool>,
    /// Whether the node has been enrolled.
    #[serde(default)]
    pub enrolled: Option<bool>,
    /// Manual steps required before bootstrap.
    #[serde(default)]
    pub required_manual_floor: Vec<String>,
    /// Free-form notes.
    #[serde(default)]
    pub notes: Option<String>,
}

// ── Leader Config ────────────────────────────────────────────────────────────

/// Leader election configuration — `[leader]`.
///
/// Not always present in the real fleet.toml but used by the election
/// subsystem. Defaults provide sensible values for the production fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderConfig {
    /// Preferred leader node name.
    #[serde(default = "default_preferred_leader")]
    pub preferred: String,
    /// Ordered fallback list.
    #[serde(default)]
    pub fallback_order: Vec<String>,
    /// Seconds between election checks.
    #[serde(default = "default_election_interval")]
    pub election_interval_secs: u64,
}

impl Default for LeaderConfig {
    fn default() -> Self {
        Self {
            preferred: default_preferred_leader(),
            fallback_order: vec![
                "james".into(),
                "marcus".into(),
                "sophie".into(),
                "priya".into(),
                "ace".into(),
            ],
            election_interval_secs: default_election_interval(),
        }
    }
}

fn default_preferred_leader() -> String {
    "taylor".into()
}
fn default_election_interval() -> u64 {
    10
}

// ── ModelConfig (backward compat) ────────────────────────────────────────────

/// Standalone model definition — kept for backward compatibility.
///
/// In the real fleet.toml, models are nested under nodes as `NodeModelConfig`.
/// This type is used by other crates that construct models programmatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    pub tier: Tier,
    #[serde(default)]
    pub params_b: f32,
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_ctx_size")]
    pub ctx_size: u32,
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub nodes: Vec<String>,
}

fn default_ctx_size() -> u32 {
    8192
}

// ─── Environment overrides ───────────────────────────────────────────────────

/// Apply `FORGEFLEET_*` environment variable overrides to a loaded config.
///
/// Supported variables:
/// - `FORGEFLEET_FLEET_NAME` → fleet.name
/// - `FORGEFLEET_API_PORT` → fleet.api_port
/// - `FORGEFLEET_HEARTBEAT_INTERVAL` → fleet.heartbeat_interval_secs
/// - `FORGEFLEET_HEARTBEAT_TIMEOUT` → fleet.heartbeat_timeout_secs
/// - `FORGEFLEET_DATABASE_MODE` → database.mode (`embedded_sqlite` | `postgres_runtime` | `postgres_full`)
/// - `FORGEFLEET_DATABASE_SQLITE_PATH` → database.sqlite_path
/// - `FORGEFLEET_DATABASE_URL` → database.url
/// - `FORGEFLEET_DATABASE_MAX_CONNECTIONS` → database.max_connections
/// - `FORGEFLEET_DATABASE_CUTOVER_EVIDENCE` → database.cutover_evidence
/// - `FORGEFLEET_PREFERRED_LEADER` → leader.preferred
/// - `FORGEFLEET_AGENT_AUTONOMOUS_MODE` → agent.autonomous_mode
/// - `FORGEFLEET_TELEGRAM_BOT_TOKEN` → transport.telegram.bot_token
/// - `FORGEFLEET_TELEGRAM_ALLOWED_CHATS` → transport.telegram.allowed_chat_ids (CSV)
/// - `FORGEFLEET_TELEGRAM_POLL_INTERVAL` → transport.telegram.polling_interval_secs
/// - `FORGEFLEET_TELEGRAM_MEDIA_DOWNLOAD_DIR` → transport.telegram.media_download_dir
/// - `FORGEFLEET_TELEGRAM_MEDIA_MAX_FILE_SIZE_BYTES` → transport.telegram.media_max_file_size_bytes
/// - `FORGEFLEET_TELEGRAM_MEDIA_ALLOWED_MIME_TYPES` → transport.telegram.media_allowed_mime_types (CSV)
pub fn apply_env_overrides(config: &mut FleetConfig) {
    if let Ok(v) = std::env::var("FORGEFLEET_FLEET_NAME") {
        info!(name = %v, "env override: fleet name");
        config.fleet.name = v;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_API_PORT")
        && let Ok(port) = v.parse::<u16>()
    {
        info!(port, "env override: API port");
        config.fleet.api_port = port;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_HEARTBEAT_INTERVAL")
        && let Ok(n) = v.parse::<u64>()
    {
        config.fleet.heartbeat_interval_secs = n;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_HEARTBEAT_TIMEOUT")
        && let Ok(n) = v.parse::<u64>()
    {
        config.fleet.heartbeat_timeout_secs = n;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_DATABASE_MODE") {
        let normalized = v.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "embedded_sqlite" | "sqlite" => {
                config.database.mode = DatabaseMode::EmbeddedSqlite;
            }
            "postgres_runtime" | "postgres" => {
                config.database.mode = DatabaseMode::PostgresRuntime;
            }
            "postgres_full" | "full_postgres" | "full-postgres" => {
                config.database.mode = DatabaseMode::PostgresFull;
            }
            _ => {
                warn!(value = %v, "invalid FORGEFLEET_DATABASE_MODE; keeping configured mode");
            }
        }
    }
    if let Ok(v) = std::env::var("FORGEFLEET_DATABASE_SQLITE_PATH")
        && !v.trim().is_empty()
    {
        config.database.sqlite_path = Some(v.trim().to_string());
    }
    if let Ok(v) = std::env::var("FORGEFLEET_DATABASE_URL") {
        info!("env override: database URL");
        config.database.url = v;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_DATABASE_MAX_CONNECTIONS")
        && let Ok(n) = v.parse::<u32>()
    {
        config.database.max_connections = n;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_DATABASE_CUTOVER_EVIDENCE")
        && !v.trim().is_empty()
    {
        config.database.cutover_evidence = Some(v.trim().to_string());
    }
    if let Ok(v) = std::env::var("FORGEFLEET_PREFERRED_LEADER") {
        config.leader.preferred = v;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_AGENT_AUTONOMOUS_MODE") {
        let enabled = matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
        config.agent.autonomous_mode = enabled;
    }

    if let Ok(v) = std::env::var("FORGEFLEET_TELEGRAM_BOT_TOKEN")
        && !v.trim().is_empty()
    {
        let telegram = config
            .transport
            .telegram
            .get_or_insert_with(TelegramTransportConfig::default);
        telegram.bot_token = Some(v.trim().to_string());
    }

    if let Ok(v) = std::env::var("FORGEFLEET_TELEGRAM_ALLOWED_CHATS") {
        let parsed = v
            .split(',')
            .filter_map(|part| part.trim().parse::<i64>().ok())
            .collect::<Vec<_>>();

        if !parsed.is_empty() {
            let telegram = config
                .transport
                .telegram
                .get_or_insert_with(TelegramTransportConfig::default);
            telegram.allowed_chat_ids = parsed;
        }
    }

    if let Ok(v) = std::env::var("FORGEFLEET_TELEGRAM_POLL_INTERVAL")
        && let Ok(secs) = v.parse::<u64>()
    {
        let telegram = config
            .transport
            .telegram
            .get_or_insert_with(TelegramTransportConfig::default);
        telegram.polling_interval_secs = secs.max(1);
    }

    if let Ok(v) = std::env::var("FORGEFLEET_TELEGRAM_MEDIA_DOWNLOAD_DIR")
        && !v.trim().is_empty()
    {
        let telegram = config
            .transport
            .telegram
            .get_or_insert_with(TelegramTransportConfig::default);
        telegram.media_download_dir = Some(v.trim().to_string());
    }

    if let Ok(v) = std::env::var("FORGEFLEET_TELEGRAM_MEDIA_MAX_FILE_SIZE_BYTES")
        && let Ok(bytes) = v.parse::<u64>()
    {
        let telegram = config
            .transport
            .telegram
            .get_or_insert_with(TelegramTransportConfig::default);
        telegram.media_max_file_size_bytes = bytes.max(1);
    }

    if let Ok(v) = std::env::var("FORGEFLEET_TELEGRAM_MEDIA_ALLOWED_MIME_TYPES") {
        let parsed = v
            .split(',')
            .map(|part| part.trim().to_ascii_lowercase())
            .filter(|mime| !mime.is_empty())
            .collect::<Vec<_>>();

        if !parsed.is_empty() {
            let telegram = config
                .transport
                .telegram
                .get_or_insert_with(TelegramTransportConfig::default);
            telegram.media_allowed_mime_types = parsed;
        }
    }
}

// ─── Load ────────────────────────────────────────────────────────────────────

/// Load and parse `fleet.toml` from the given path, applying env overrides.
pub fn load_config(path: &Path) -> Result<FleetConfig> {
    if !path.exists() {
        return Err(ForgeFleetError::ConfigNotFound {
            path: path.to_path_buf(),
        });
    }
    let raw = std::fs::read_to_string(path)?;
    let mut config: FleetConfig = toml::from_str(&raw)?;
    apply_env_overrides(&mut config);
    info!(
        path = %path.display(),
        nodes = config.nodes.len(),
        "loaded fleet config"
    );
    Ok(config)
}

/// Search standard paths for fleet.toml and load the first one found.
///
/// Search order:
/// 1. `$FORGEFLEET_CONFIG` (explicit override)
/// 2. `./fleet.toml`
/// 3. `~/.forgefleet/fleet.toml`
/// 4. `~/.config/forgefleet/fleet.toml`
/// 5. `/etc/forgefleet/fleet.toml`
pub fn load_config_auto() -> Result<(FleetConfig, PathBuf)> {
    let candidates: Vec<PathBuf> = vec![
        std::env::var("FORGEFLEET_CONFIG").ok().map(PathBuf::from),
        Some(PathBuf::from("fleet.toml")),
        home_forgefleet().map(|d| d.join("fleet.toml")),
        config_dir().map(|d| d.join("fleet.toml")),
        Some(PathBuf::from("/etc/forgefleet/fleet.toml")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for path in &candidates {
        if path.exists() {
            let cfg = load_config(path)?;
            return Ok((cfg, path.clone()));
        }
    }

    Err(ForgeFleetError::ConfigNotFound {
        path: PathBuf::from("fleet.toml"),
    })
}

/// Helper: return `~/.forgefleet` if `$HOME` is set.
fn home_forgefleet() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".forgefleet"))
}

/// Helper: return `~/.config/forgefleet` if `$HOME` is set.
fn config_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config").join("forgefleet"))
}

// ─── Hot-reload handle ───────────────────────────────────────────────────────

/// Thread-safe, atomically-swappable config handle.
///
/// Consumers clone the `Arc<FleetConfig>` from the watch channel
/// and get notified on updates.
#[derive(Clone)]
pub struct ConfigHandle {
    rx: watch::Receiver<Arc<FleetConfig>>,
    /// Cached overrides from runtime (e.g., CLI flags) that survive reloads.
    overrides: Arc<DashMap<String, String>>,
    path: PathBuf,
}

impl ConfigHandle {
    /// Create a new handle from an initial config and its file path.
    pub fn new(config: FleetConfig, path: PathBuf) -> (Self, watch::Sender<Arc<FleetConfig>>) {
        let (tx, rx) = watch::channel(Arc::new(config));
        let handle = Self {
            rx,
            overrides: Arc::new(DashMap::new()),
            path,
        };
        (handle, tx)
    }

    /// Get a snapshot of the current config.
    pub fn get(&self) -> Arc<FleetConfig> {
        self.rx.borrow().clone()
    }

    /// Wait for the config to change and return the new value.
    pub async fn changed(&mut self) -> Arc<FleetConfig> {
        // Ignore the result — if the sender dropped we still return last known.
        let _ = self.rx.changed().await;
        self.rx.borrow().clone()
    }

    /// Set a runtime override that persists across reloads.
    pub fn set_override(&self, key: impl Into<String>, value: impl Into<String>) {
        self.overrides.insert(key.into(), value.into());
    }

    /// Get the config file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reload config from disk, apply env overrides and runtime overrides,
    /// then publish to all subscribers.
    pub fn reload(&self, tx: &watch::Sender<Arc<FleetConfig>>) -> Result<()> {
        let mut config = {
            let raw = std::fs::read_to_string(&self.path)?;
            let cfg: FleetConfig = toml::from_str(&raw)?;
            cfg
        };
        apply_env_overrides(&mut config);

        // Apply runtime overrides on top of env overrides.
        for entry in self.overrides.iter() {
            match entry.key().as_str() {
                "fleet.name" => config.fleet.name = entry.value().clone(),
                "fleet.api_port" => {
                    if let Ok(p) = entry.value().parse::<u16>() {
                        config.fleet.api_port = p;
                    }
                }
                "database.mode" => {
                    let normalized = entry.value().trim().to_ascii_lowercase();
                    config.database.mode = match normalized.as_str() {
                        "postgres_runtime" | "postgres" => DatabaseMode::PostgresRuntime,
                        "postgres_full" | "full_postgres" | "full-postgres" => {
                            DatabaseMode::PostgresFull
                        }
                        _ => DatabaseMode::EmbeddedSqlite,
                    };
                }
                "database.sqlite_path" => {
                    let value = entry.value().trim();
                    config.database.sqlite_path = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "database.url" => config.database.url = entry.value().clone(),
                "database.cutover_evidence" => {
                    let value = entry.value().trim();
                    config.database.cutover_evidence = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                other => {
                    warn!(key = other, "unknown runtime override — ignoring");
                }
            }
        }

        tx.send(Arc::new(config))
            .map_err(|_| ForgeFleetError::Internal("config watch channel closed".into()))?;
        info!(path = %self.path.display(), "config reloaded");
        Ok(())
    }
}

// ─── File watcher ────────────────────────────────────────────────────────────

/// Spawn a file watcher that reloads config on changes.
///
/// Uses simple polling (500ms interval) since `notify` is not a dependency.
pub fn spawn_watcher(
    handle: ConfigHandle,
    tx: watch::Sender<Arc<FleetConfig>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_modified = std::fs::metadata(&handle.path)
            .and_then(|m| m.modified())
            .ok();

        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let current = std::fs::metadata(&handle.path)
                .and_then(|m| m.modified())
                .ok();

            if current != last_modified {
                last_modified = current;
                match handle.reload(&tx) {
                    Ok(()) => info!("config hot-reloaded"),
                    Err(e) => warn!(error = %e, "config reload failed — keeping previous"),
                }
            }
        }
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Sample TOML matching the real production fleet.toml format.
    fn sample_toml() -> &'static str {
        r#"
[general]
name = "TestFleet"
version = "1.0"
default_repo = "/tmp/test-repo"

[nodes.taylor]
ip = "192.168.5.100"
ssh_user = "venkat"
ram_gb = 96
cpu_cores = 32
os = "macOS 26.3"
role = "gateway"
alt_ips = ["192.168.5.101"]
election_priority = 1

[nodes.taylor.models.qwen35_35b]
name = "Qwen3.5-35B"
family = "qwen"
port = 55000
tier = 2
local = true
lifecycle = "production"
mode = "on_demand"
preferred_workloads = ["fallback_reasoning", "control_assist"]

[nodes.taylor.resources]
cpu_cores = 32
ram_gb = 96
vram_gb = 0

[nodes.taylor.capabilities]
control_plane = true
model_building = false
premium_inference = false
local_inference = true
docker = true

[nodes.taylor.preferences]
preferred_workloads = ["control", "coordination"]
first_preference_workloads = []

[nodes.marcus]
ip = "192.168.5.102"
ssh_user = "marcus"
ram_gb = 32
cpu_cores = 8
os = "Ubuntu 24.04"
role = "builder"
alt_ips = []

[nodes.marcus.models.code_model]
name = "Qwen3.5-Coder-32B"
family = "qwen"
port = 55002
tier = 2
local = true
lifecycle = "production"
mode = "always_on"
preferred_workloads = ["coding", "review", "build"]

[notifications.telegram]
chat_id = "8496613333"
channel = "telegram"

[transport.telegram]
enabled = true
bot_token_env = "FORGEFLEET_TELEGRAM_BOT_TOKEN"
allowed_chat_ids = [8496613333, 8622294597]
polling_interval_secs = 2
polling_timeout_secs = 15
media_download_dir = "/tmp/forgefleet-telegram"
media_max_file_size_bytes = 10485760
media_allowed_mime_types = ["image/*", "video/mp4"]

[services.mc]
port = 60002

[services.forgefleet]
port = 51820

[llm]
ports = [51800, 51801, 51802, 51803]
announce_port = 50099

[llm.timeouts]
tier1 = 120
tier2 = 300
tier3 = 600
tier4 = 900

[ports]
openclaw = 50000
forgefleet = 50001
model_start = 55000
model_end = 55010

[scheduling]
canonical_writer = "taylor"
degraded_coordinator_mode = true
max_handoffs_per_task = 2

[mcp.openclaw]
server = true
client = true
port = 50000
endpoint = "http://127.0.0.1:50000/mcp"
required = true
required_tools = ["fleet_status"]
optional_tools = ["model_stats"]
depends_on = ["forgefleet"]
optional_depends_on = ["openclaw"]
request_timeout_secs = 3

[loops.evolution]
enabled = true
interval_secs = 60
minimum_improvement_ratio = 0.15

[loops.updater]
enabled = true
check_interval_secs = 1800
auto_apply = false
repo_path = "/tmp/test-repo"
current_binary_path = "/tmp/forgefleetd"
git_remote = "origin"
git_branch = "main"

[loops.self_heal]
enabled = true
interval_secs = 20
auto_adopt = true
max_health_failures = 4
health_probe_timeout_secs = 6
stop_timeout_secs = 12

[loops.mcp_federation]
enabled = true
interval_secs = 90
request_timeout_secs = 4

[enrollment]
bootstrap_engine = "forgefleet"
bootstrap_interface = "openclaw"
require_ssh_before_bootstrap = true
auto_enroll_after_healthcheck = true

[database]
mode = "postgres_runtime"
sqlite_path = "./forgefleet.db"
host = "127.0.0.1"
port = 55432
name = "forgefleet"
user = "forgefleet"
password = "forgefleet"
url = "postgresql://forgefleet:forgefleet@127.0.0.1:55432/forgefleet"

[leader]
preferred = "taylor"
fallback_order = ["marcus"]

[[bootstrap_targets]]
name = "logan"
status = "in_progress"
os = "Ubuntu"
reachable_by_ssh = true
enrolled = false
required_manual_floor = ["network", "ssh"]
notes = "Setup started."
"#
    }

    #[test]
    fn test_parse_fleet_toml() {
        let config: FleetConfig = toml::from_str(sample_toml()).unwrap();
        assert_eq!(config.fleet.name, "TestFleet");
        assert_eq!(config.fleet.version.as_deref(), Some("1.0"));
        assert_eq!(config.fleet.default_repo.as_deref(), Some("/tmp/test-repo"));
        assert_eq!(config.nodes.len(), 2);

        let taylor = config.nodes.get("taylor").expect("taylor node");
        assert_eq!(taylor.ip, "192.168.5.100");
        assert_eq!(taylor.role, Role::Gateway);
        assert_eq!(taylor.ssh_user.as_deref(), Some("venkat"));
        assert_eq!(taylor.ram_gb, Some(96));
        assert_eq!(taylor.os.as_deref(), Some("macOS 26.3"));
        assert_eq!(taylor.alt_ips, vec!["192.168.5.101".to_string()]);
        assert_eq!(taylor.priority(), 1);

        // Check nested models.
        let qwen = taylor.models.get("qwen35_35b").expect("qwen model");
        assert_eq!(qwen.name, "Qwen3.5-35B");
        assert_eq!(qwen.family.as_deref(), Some("qwen"));
        assert_eq!(qwen.tier, 2);
        assert_eq!(qwen.port, Some(55000));
        assert_eq!(qwen.lifecycle.as_deref(), Some("production"));
        assert_eq!(
            qwen.preferred_workloads,
            vec!["fallback_reasoning", "control_assist"]
        );

        // Check resources.
        let res = taylor.resources.as_ref().expect("resources");
        assert_eq!(res.ram_gb, Some(96));
        assert_eq!(res.cpu_cores, Some(32));
        assert_eq!(res.vram_gb, Some(0));

        // Check capabilities.
        let caps = taylor.capabilities.as_ref().expect("capabilities");
        assert_eq!(caps.control_plane, Some(true));
        assert_eq!(caps.docker, Some(true));

        // Check marcus.
        let marcus = config.nodes.get("marcus").expect("marcus node");
        assert_eq!(marcus.ip, "192.168.5.102");
        assert_eq!(marcus.role, Role::Builder);

        // Check notifications.
        let tg = config.notifications.telegram.as_ref().expect("telegram");
        assert_eq!(tg.chat_id, "8496613333");

        // Check telegram transport.
        let transport = config
            .transport
            .telegram
            .as_ref()
            .expect("transport.telegram");
        assert!(transport.enabled);
        assert_eq!(transport.allowed_chat_ids, vec![8496613333, 8622294597]);
        assert_eq!(transport.polling_interval_secs, 2);
        assert_eq!(transport.polling_timeout_secs, 15);
        assert_eq!(
            transport.media_download_dir.as_deref(),
            Some("/tmp/forgefleet-telegram")
        );
        assert_eq!(transport.media_max_file_size_bytes, 10_485_760);
        assert_eq!(
            transport.media_allowed_mime_types,
            vec!["image/*".to_string(), "video/mp4".to_string()]
        );

        // Check services.
        assert_eq!(config.services.get("mc").unwrap().port, 60002);

        // Check LLM config.
        assert_eq!(config.llm.ports, vec![51800, 51801, 51802, 51803]);
        assert_eq!(config.llm.timeouts.tier1, Some(120));

        // Check ports.
        assert_eq!(config.ports.openclaw, Some(50000));

        // Check scheduling.
        assert_eq!(
            config.scheduling.canonical_writer.as_deref(),
            Some("taylor")
        );

        // Check MCP.
        let mcp_openclaw = config.mcp.get("openclaw").unwrap();
        assert_eq!(mcp_openclaw.port, Some(50000));
        assert_eq!(
            mcp_openclaw.endpoint.as_deref(),
            Some("http://127.0.0.1:50000/mcp")
        );
        assert_eq!(mcp_openclaw.required, Some(true));
        assert_eq!(
            mcp_openclaw.required_tools,
            vec!["fleet_status".to_string()]
        );

        // Check loop settings.
        assert!(config.loops.evolution.enabled);
        assert_eq!(config.loops.evolution.interval_secs, 60);
        assert_eq!(config.loops.updater.check_interval_secs, 1800);
        assert_eq!(config.loops.self_heal.max_health_failures, 4);
        assert_eq!(config.loops.mcp_federation.request_timeout_secs, 4);

        // Check enrollment.
        assert_eq!(
            config.enrollment.bootstrap_engine.as_deref(),
            Some("forgefleet")
        );

        // Check database.
        assert_eq!(config.database.mode, DatabaseMode::PostgresRuntime);
        assert_eq!(
            config.database.sqlite_path.as_deref(),
            Some("./forgefleet.db")
        );
        assert_eq!(config.database.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(config.database.port, Some(55432));
        assert_eq!(config.database.name.as_deref(), Some("forgefleet"));

        // Check leader.
        assert_eq!(config.leader.preferred, "taylor");

        // Check bootstrap_targets.
        assert_eq!(config.bootstrap_targets.len(), 1);
        assert_eq!(config.bootstrap_targets[0].name, "logan");
    }

    #[test]
    fn test_load_config_from_file() {
        let dir = std::env::temp_dir().join("ff-core-test-config-v2");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("fleet.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(sample_toml().as_bytes()).unwrap();
        drop(f);

        let config = load_config(&path).unwrap();
        assert_eq!(config.nodes.len(), 2);
        assert!(config.nodes.contains_key("taylor"));

        // Cleanup.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_config_not_found() {
        let result = load_config(Path::new("/nonexistent/fleet.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_defaults() {
        let config: FleetConfig = toml::from_str("").unwrap();
        assert_eq!(config.fleet.name, "ForgeFleet");
        assert_eq!(config.fleet.heartbeat_interval_secs, 15);
        assert_eq!(config.fleet.api_port, 51800);
        assert_eq!(config.leader.preferred, "taylor");
        assert_eq!(config.database.max_connections, 10);
        assert_eq!(config.database.mode, DatabaseMode::EmbeddedSqlite);
        assert!(config.loops.evolution.enabled);
        assert!(config.loops.self_heal.enabled);
        assert!(config.nodes.is_empty());
        assert!(config.models.is_empty());
        assert!(config.transport.telegram.is_none());
    }

    #[test]
    fn test_database_mode_postgres_full_from_toml() {
        let config: FleetConfig = toml::from_str(
            r#"
[database]
mode = "postgres_full"
url = "postgresql://forgefleet:forgefleet@127.0.0.1:55432/forgefleet"
cutover_evidence = "CUTOVER-2026-04-05"
"#,
        )
        .unwrap();

        assert_eq!(config.database.mode, DatabaseMode::PostgresFull);
        assert_eq!(config.database.mode.as_str(), "postgres_full");
        assert_eq!(
            config.database.cutover_evidence_ref(),
            Some("CUTOVER-2026-04-05")
        );
    }

    #[test]
    fn test_database_mode_postgres_full_aliases() {
        for alias in ["full_postgres", "full-postgres"] {
            let raw = format!(
                r#"
[database]
mode = "{alias}"
"#
            );

            let config: FleetConfig = toml::from_str(&raw).unwrap();
            assert_eq!(config.database.mode, DatabaseMode::PostgresFull);
        }
    }

    #[test]
    fn test_roundtrip() {
        let config: FleetConfig = toml::from_str(sample_toml()).unwrap();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let reparsed: FleetConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(config.fleet.name, reparsed.fleet.name);
        assert_eq!(config.nodes.len(), reparsed.nodes.len());
    }

    #[tokio::test]
    async fn test_config_handle_get() {
        let config: FleetConfig = toml::from_str(sample_toml()).unwrap();
        let (handle, _tx) = ConfigHandle::new(config, PathBuf::from("fleet.toml"));
        let snap = handle.get();
        assert_eq!(snap.fleet.name, "TestFleet");
    }

    #[test]
    fn test_node_convenience_methods() {
        let config: FleetConfig = toml::from_str(sample_toml()).unwrap();
        assert!(config.get_node("taylor").is_some());
        assert!(config.get_node("nonexistent").is_none());

        let all_models = config.all_node_models();
        assert!(!all_models.is_empty());
        // Should find qwen35_35b on taylor.
        assert!(
            all_models
                .iter()
                .any(|(node, slug, _)| *node == "taylor" && *slug == "qwen35_35b")
        );
    }

    #[test]
    fn test_effective_ram_and_cpu() {
        let config: FleetConfig = toml::from_str(sample_toml()).unwrap();
        let taylor = config.get_node("taylor").unwrap();
        assert_eq!(taylor.effective_ram_gb(), Some(96));
        assert_eq!(taylor.effective_cpu_cores(), Some(32));
    }

    #[test]
    fn test_telegram_transport_token_resolution_prefers_inline_token() {
        let config: FleetConfig = toml::from_str(sample_toml()).unwrap();
        let transport = config.transport.telegram.expect("transport.telegram");

        let overridden = TelegramTransportConfig {
            bot_token: Some("abc123".to_string()),
            ..transport
        };

        assert_eq!(overridden.resolve_bot_token().as_deref(), Some("abc123"));
    }

    #[test]
    fn test_telegram_transport_chat_allowlist() {
        let transport = TelegramTransportConfig {
            enabled: true,
            allowed_chat_ids: vec![123, 456],
            ..Default::default()
        };

        assert!(transport.is_chat_allowed(123));
        assert!(!transport.is_chat_allowed(999));
    }

    /// Parse real production fleet.toml if available (integration test).
    #[test]
    fn test_parse_production_fleet_toml() {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = PathBuf::from(&home).join(".forgefleet").join("fleet.toml");
        if path.exists() {
            let config = load_config(&path).unwrap();
            assert!(!config.fleet.name.is_empty());
            // Should have at least one node.
            assert!(!config.nodes.is_empty());
        }
    }
}
