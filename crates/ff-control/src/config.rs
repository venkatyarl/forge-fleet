//! Control-plane configuration options.
//!
//! This module holds ff-control-specific settings that are not part of the
//! shared `ff_core::config::FleetConfig` but which the control plane needs to
//! tune its behavior.

use serde::{Deserialize, Serialize};

/// Alert deduplication configuration.
///
/// Controls whether repeated alerts are suppressed within a time-to-live (TTL)
/// window and how that window is sized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeduplicationConfig {
    /// Enable alert deduplication.
    ///
    /// When `true`, alerts with the same deduplication key that arrive within
    /// `ttl_secs` of the last emitted alert are suppressed.
    #[serde(default = "default_deduplication_enabled")]
    pub enabled: bool,

    /// Time-to-live in seconds for a deduplication entry.
    ///
    /// After this duration passes, the next matching alert will be emitted
    /// again and a new entry is created.
    #[serde(default = "default_deduplication_ttl_secs")]
    pub ttl_secs: u64,
}

impl DeduplicationConfig {
    /// Whether deduplication is currently active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Return the TTL as a `std::time::Duration`.
    pub fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.ttl_secs)
    }
}

impl Default for DeduplicationConfig {
    fn default() -> Self {
        Self {
            enabled: default_deduplication_enabled(),
            ttl_secs: default_deduplication_ttl_secs(),
        }
    }
}

fn default_deduplication_enabled() -> bool {
    false
}

fn default_deduplication_ttl_secs() -> u64 {
    300
}

/// Top-level control-plane configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    /// Maximum build wall-clock duration in seconds.
    #[serde(default = "default_max_build_duration_secs")]
    pub max_build_duration_secs: u64,

    /// Alert deduplication settings — `[control.alerts.deduplication]`.
    #[serde(default)]
    pub alerts: AlertConfig,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            max_build_duration_secs: default_max_build_duration_secs(),
            alerts: AlertConfig::default(),
        }
    }
}

fn default_max_build_duration_secs() -> u64 {
    300
}

/// Alert handling configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlertConfig {
    /// Deduplication settings for emitted alerts.
    #[serde(default)]
    pub deduplication: DeduplicationConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplication_config_defaults() {
        let cfg = DeduplicationConfig::default();
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.ttl_secs, 300);
        assert_eq!(cfg.ttl(), std::time::Duration::from_secs(300));
    }

    #[test]
    fn deduplication_config_roundtrip() {
        let cfg = DeduplicationConfig {
            enabled: true,
            ttl_secs: 60,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: DeduplicationConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_enabled());
        assert_eq!(parsed.ttl_secs, 60);
    }

    #[test]
    fn control_config_defaults_max_build_duration() {
        let cfg = ControlConfig::default();
        assert_eq!(cfg.max_build_duration_secs, 300);

        let parsed: ControlConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.max_build_duration_secs, 300);
    }
}
