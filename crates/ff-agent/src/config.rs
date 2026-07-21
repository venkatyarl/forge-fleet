use std::path::PathBuf;

use ff_core::ActivityLevel;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub node_id: String,
    pub leader_url: String,
    pub runtime_url: String,
    pub http_port: u16,
    pub heartbeat_interval_secs: u64,
    pub activity_poll_interval_secs: u64,
    pub task_poll_interval_secs: u64,
    pub activity_override: Option<ActivityLevel>,
    /// Max wall-clock a build shell-command may run before the background
    /// timeout monitor kills it. Maps to `max_build_duration` in the fleet
    /// config (seconds). `0` disables the monitor.
    pub max_build_duration_secs: u64,
    /// How often the build timeout monitor scans running builds.
    pub build_monitor_poll_secs: u64,
    pub slm_model: Option<PathBuf>,
    pub slm_threads: Option<usize>,
    pub slm_mem_budget_mb: Option<u64>,
    pub log_monitor: LogMonitoringConfig,
}

impl AgentConfig {
    pub fn from_env() -> Self {
        Self {
            node_id: std::env::var("FF_AGENT_NODE_ID")
                .unwrap_or_else(|_| format!("node-{}", uuid::Uuid::new_v4())),
            leader_url: std::env::var("FF_LEADER_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:51819".to_string()),
            runtime_url: std::env::var("FF_RUNTIME_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:55000".to_string()),
            http_port: std::env::var("FF_AGENT_HTTP_PORT")
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(51820),
            heartbeat_interval_secs: std::env::var("FF_AGENT_HEARTBEAT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(15),
            activity_poll_interval_secs: std::env::var("FF_AGENT_ACTIVITY_POLL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5),
            task_poll_interval_secs: std::env::var("FF_AGENT_TASK_POLL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(8),
            activity_override: std::env::var("FF_AGENT_ACTIVITY_OVERRIDE")
                .ok()
                .and_then(|v| parse_activity_level(&v)),
            max_build_duration_secs: std::env::var("FF_AGENT_MAX_BUILD_DURATION_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(300),
            build_monitor_poll_secs: std::env::var("FF_AGENT_BUILD_MONITOR_POLL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(10),
            slm_model: std::env::var_os("FORGEFLEET_SLM_MODEL").map(PathBuf::from),
            slm_threads: std::env::var("FORGEFLEET_SLM_THREADS")
                .ok()
                .and_then(|value| value.parse().ok()),
            slm_mem_budget_mb: std::env::var("FORGEFLEET_SLM_MEM_BUDGET_MB")
                .ok()
                .and_then(|value| value.parse().ok()),
            log_monitor: LogMonitoringConfig::from_env(),
        }
    }
}

/// Log monitoring configuration for the local agent daemon.
///
/// Watches configured log files for recurring patterns and dispatches
/// notifications when a recurrence threshold is crossed within a lookback
/// window.
#[derive(Debug, Clone)]
pub struct LogMonitoringConfig {
    /// Enable log monitoring.
    pub enabled: bool,
    /// Log file paths to watch. Supports comma-separated paths in the
    /// `FF_AGENT_LOG_MONITOR_PATHS` environment variable.
    pub log_paths: Vec<PathBuf>,
    /// Number of matching log lines required to trigger an alert.
    pub recurrence_threshold: u32,
    /// Lookback window in seconds for recurrence counting.
    pub recurrence_window_secs: u64,
    /// Polling interval in seconds.
    pub poll_interval_secs: u64,
    /// Notification channels to use when a recurrence threshold is crossed
    /// (e.g. "telegram", "slack", "desktop", "webhook").
    pub notification_channels: Vec<String>,
}

impl LogMonitoringConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var("FF_AGENT_LOG_MONITOR_ENABLED")
                .ok()
                .map(|v| v.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            log_paths: std::env::var("FF_AGENT_LOG_MONITOR_PATHS")
                .ok()
                .map(|v| parse_path_list(&v))
                .unwrap_or_else(default_log_paths),
            recurrence_threshold: std::env::var("FF_AGENT_LOG_MONITOR_RECURRENCE_THRESHOLD")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(5),
            recurrence_window_secs: std::env::var("FF_AGENT_LOG_MONITOR_WINDOW_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(300),
            poll_interval_secs: std::env::var("FF_AGENT_LOG_MONITOR_POLL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60),
            notification_channels: std::env::var("FF_AGENT_LOG_MONITOR_CHANNELS")
                .ok()
                .map(|v| parse_string_list(&v))
                .unwrap_or_default(),
        }
    }
}

fn default_log_paths() -> Vec<PathBuf> {
    dirs::home_dir()
        .map(|home| {
            let logs = home.join(".forgefleet").join("logs");
            vec![
                logs.join("forgefleetd.log"),
                logs.join("forgefleetd.out.log"),
                logs.join("forgefleetd.err.log"),
            ]
        })
        .unwrap_or_default()
}

fn parse_path_list(raw: &str) -> Vec<PathBuf> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn parse_string_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn parse_activity_level(raw: &str) -> Option<ActivityLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "interactive" => Some(ActivityLevel::Interactive),
        "assist" => Some(ActivityLevel::Assist),
        "idle" => Some(ActivityLevel::Idle),
        "protected" => Some(ActivityLevel::Protected),
        _ => None,
    }
}
