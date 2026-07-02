use std::time::Duration;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub enum TickScope {
    LeaderGated,
    PerHost,
}

#[derive(Debug, Clone)]
pub struct GateSecret(pub String);

#[derive(Debug, Clone)]
pub struct JitterConfig {
    pub enabled: bool,
    pub min_secs: u64,
    pub max_secs: u64,
}

#[derive(Debug, Clone)]
pub struct TickExecutionMetrics {
    pub last_run: Option<DateTime<Utc>>,
    pub duration: Option<Duration>,
    pub outcome: Option<String>,
}
