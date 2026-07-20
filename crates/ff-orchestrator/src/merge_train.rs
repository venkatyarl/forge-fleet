//! Merge train configuration for staged, validated rollouts.
//!
//! A merge train queues proposed changes, validates each candidate against the
//! current target plus every preceding candidate, and merges them sequentially
//! only when all checks pass. This module holds the tunables that control queue
//! depth, retry behaviour, timeout windows, and validation policies.

use serde::{Deserialize, Serialize};

/// Configuration for a merge train.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeTrainConfig {
    /// Maximum number of merge candidates that may be queued in the train.
    pub max_queue_depth: usize,
    /// Maximum number of validation attempts for a single candidate before it
    /// is removed from the train.
    pub max_retries: u32,
    /// Seconds to wait for validation (build / test / checks) to complete.
    pub validation_timeout_secs: u64,
    /// Seconds to wait between retry attempts after a validation failure.
    pub retry_backoff_secs: u64,
    /// Whether the train requires a green validation before a candidate may
    /// join at the end of the queue.
    pub require_validation_to_enqueue: bool,
    /// Whether the train should automatically rebase each candidate on top of
    /// the previous successfully merged candidate.
    pub rebase_each_candidate: bool,
    /// Whether to allow emergency bypass of the train for hotfix branches.
    pub allow_hotfix_bypass: bool,
    /// Maximum age, in seconds, after which a stale candidate is evicted.
    pub candidate_ttl_secs: u64,
}

impl Default for MergeTrainConfig {
    fn default() -> Self {
        Self {
            max_queue_depth: 10,
            max_retries: 3,
            validation_timeout_secs: 1800,
            retry_backoff_secs: 60,
            require_validation_to_enqueue: true,
            rebase_each_candidate: true,
            allow_hotfix_bypass: false,
            candidate_ttl_secs: 86400,
        }
    }
}

impl MergeTrainConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cfg = MergeTrainConfig::new();
        assert_eq!(cfg.max_queue_depth, 10);
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.validation_timeout_secs, 1800);
        assert_eq!(cfg.retry_backoff_secs, 60);
        assert!(cfg.require_validation_to_enqueue);
        assert!(cfg.rebase_each_candidate);
        assert!(!cfg.allow_hotfix_bypass);
        assert_eq!(cfg.candidate_ttl_secs, 86400);
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = MergeTrainConfig::new();
        let json = serde_json::to_string(&cfg).expect("serialize");
        let restored: MergeTrainConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cfg, restored);
    }
}
