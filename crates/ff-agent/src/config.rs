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
}

impl AgentConfig {
    pub fn from_env() -> Self {
        Self {
            node_id: std::env::var("FF_AGENT_NODE_ID")
                .unwrap_or_else(|_| format!("node-{}", uuid::Uuid::new_v4())),
            leader_url: std::env::var("FF_LEADER_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:51819".to_string()),
            runtime_url: std::env::var("FF_RUNTIME_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8000".to_string()),
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
        }
    }
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
