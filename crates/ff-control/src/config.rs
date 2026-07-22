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

/// Escalation to the 480B model tier configuration.
///
/// Controls when the control plane escalates work to the 480B model and which
/// endpoint serves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationConfig {
    /// Enable escalation to the 480B tier.
    ///
    /// When `false`, work is never escalated regardless of thresholds.
    #[serde(default = "default_escalation_enabled")]
    pub enabled: bool,

    /// Consecutive failures on lower tiers before escalating to the 480B tier.
    #[serde(default = "default_escalation_failure_threshold")]
    pub failure_threshold: u32,

    /// Task complexity score (0.0–1.0) at or above which work escalates
    /// directly to the 480B tier without first exhausting lower tiers.
    #[serde(default = "default_escalation_complexity_threshold")]
    pub complexity_threshold: f64,

    /// Endpoint serving the 480B model.
    #[serde(default)]
    pub endpoint: Endpoint480bConfig,
}

impl EscalationConfig {
    /// Whether escalation is currently active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: default_escalation_enabled(),
            failure_threshold: default_escalation_failure_threshold(),
            complexity_threshold: default_escalation_complexity_threshold(),
            endpoint: Endpoint480bConfig::default(),
        }
    }
}

fn default_escalation_enabled() -> bool {
    false
}

fn default_escalation_failure_threshold() -> u32 {
    2
}

fn default_escalation_complexity_threshold() -> f64 {
    0.85
}

/// 480B model endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint480bConfig {
    /// Base URL of the inference server hosting the 480B model.
    ///
    /// Empty means no endpoint is configured and escalation cannot dispatch.
    #[serde(default)]
    pub url: String,

    /// Model identifier to request from the endpoint.
    #[serde(default = "default_endpoint_480b_model")]
    pub model: String,

    /// Request timeout in seconds for calls to the 480B endpoint.
    #[serde(default = "default_endpoint_480b_timeout_secs")]
    pub timeout_secs: u64,
}

impl Endpoint480bConfig {
    /// Whether an endpoint URL has been configured.
    pub fn is_configured(&self) -> bool {
        !self.url.is_empty()
    }

    /// Return the request timeout as a `std::time::Duration`.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }
}

impl Default for Endpoint480bConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            model: default_endpoint_480b_model(),
            timeout_secs: default_endpoint_480b_timeout_secs(),
        }
    }
}

fn default_endpoint_480b_model() -> String {
    "qwen3-coder-480b".to_string()
}

fn default_endpoint_480b_timeout_secs() -> u64 {
    600
}

/// Top-level control-plane configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    /// Maximum wall-clock duration for a single build, in seconds.
    #[serde(default = "default_max_build_duration_secs")]
    pub max_build_duration_secs: u64,

    /// Alert deduplication settings — `[control.alerts.deduplication]`.
    #[serde(default)]
    pub alerts: AlertConfig,

    /// 480B escalation settings — `[control.escalation]`.
    #[serde(default)]
    pub escalation: EscalationConfig,

    /// Per-project slot allocation settings — `[control.slot_allocation]`.
    #[serde(default)]
    pub slot_allocation: crate::slot_allocation::SlotAllocationConfig,
}

impl ControlConfig {
    /// Return the maximum build duration as a `std::time::Duration`.
    pub fn max_build_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.max_build_duration_secs)
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            max_build_duration_secs: default_max_build_duration_secs(),
            alerts: AlertConfig::default(),
            escalation: EscalationConfig::default(),
            slot_allocation: crate::slot_allocation::SlotAllocationConfig::default(),
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
    fn control_config_max_build_duration_defaults() {
        let cfg: ControlConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.max_build_duration_secs, 300);
        assert_eq!(
            cfg.max_build_duration(),
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn control_config_max_build_duration_deserializes() {
        let cfg: ControlConfig =
            serde_json::from_str(r#"{"max_build_duration_secs": 900}"#).unwrap();
        assert_eq!(cfg.max_build_duration_secs, 900);
        assert_eq!(
            cfg.max_build_duration(),
            std::time::Duration::from_secs(900)
        );
    }

    #[test]
    fn deduplication_config_defaults() {
        let cfg = DeduplicationConfig::default();
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.ttl_secs, 300);
        assert_eq!(cfg.ttl(), std::time::Duration::from_secs(300));
    }

    #[test]
    fn escalation_config_defaults() {
        let cfg = EscalationConfig::default();
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.failure_threshold, 2);
        assert_eq!(cfg.complexity_threshold, 0.85);
        assert!(!cfg.endpoint.is_configured());
        assert_eq!(cfg.endpoint.model, "qwen3-coder-480b");
        assert_eq!(cfg.endpoint.timeout_secs, 600);
        assert_eq!(cfg.endpoint.timeout(), std::time::Duration::from_secs(600));
    }

    #[test]
    fn escalation_config_roundtrip() {
        let cfg = EscalationConfig {
            enabled: true,
            failure_threshold: 3,
            complexity_threshold: 0.9,
            endpoint: Endpoint480bConfig {
                url: "http://127.0.0.1:51001".to_string(),
                model: "qwen3-coder-480b-a35b".to_string(),
                timeout_secs: 900,
            },
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: EscalationConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_enabled());
        assert_eq!(parsed.failure_threshold, 3);
        assert_eq!(parsed.complexity_threshold, 0.9);
        assert!(parsed.endpoint.is_configured());
        assert_eq!(parsed.endpoint.url, "http://127.0.0.1:51001");
        assert_eq!(parsed.endpoint.timeout_secs, 900);
    }

    #[test]
    fn escalation_config_deserializes_from_empty_object() {
        let parsed: EscalationConfig = serde_json::from_str("{}").unwrap();
        assert!(!parsed.is_enabled());
        assert_eq!(parsed.failure_threshold, 2);
        assert!(!parsed.endpoint.is_configured());
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
}
