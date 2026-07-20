//! Deployment configuration for the `ff-deploy` crate.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default time to wait for active leases to drain before restarting a daemon.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Top-level deployment configuration.
///
/// Controls orchestration behavior such as how long to wait for leases and
/// in-flight work to drain before restarting a service or node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployConfig {
    /// How long to wait for leases to release before proceeding with restart.
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout: Duration,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            drain_timeout: default_drain_timeout(),
        }
    }
}

fn default_drain_timeout() -> Duration {
    DEFAULT_DRAIN_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_drain_timeout_is_five_minutes() {
        let cfg = DeployConfig::default();
        assert_eq!(cfg.drain_timeout, DEFAULT_DRAIN_TIMEOUT);
    }

    #[test]
    fn deserialize_uses_default_when_field_missing() {
        let cfg: DeployConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.drain_timeout, DEFAULT_DRAIN_TIMEOUT);
    }

    #[test]
    fn deserialize_override() {
        let cfg: DeployConfig =
            serde_json::from_str(r#"{"drain_timeout":{"secs":60,"nanos":0}}"#).unwrap();
        assert_eq!(cfg.drain_timeout, Duration::from_secs(60));
    }
}
