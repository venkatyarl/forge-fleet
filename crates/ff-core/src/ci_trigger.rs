//! CI pipeline trigger metadata and merge-train detection.
//!
//! This module provides the shared primitive used to decide whether a CI run was
//! triggered by a regular pull-request update or by a merge-train (GitHub merge
//! group) event. Downstream crates such as `ff-gateway` can populate this struct
//! from webhook payloads and persist the `is_merge_train` flag with the CI run.

use serde::{Deserialize, Serialize};

/// Metadata describing why a CI pipeline was triggered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CiPipelineTrigger {
    /// Git branch that triggered the CI run.
    pub branch: String,
    /// Pull-request number, if the trigger was associated with a PR.
    pub pr_number: Option<u64>,
    /// Underlying GitHub/GitLab event type (e.g. `pull_request`, `merge_group`).
    pub event: String,
    /// `true` when this trigger belongs to a merge train / merge group.
    pub is_merge_train: bool,
}

impl CiPipelineTrigger {
    /// Returns `true` when `branch` or `event` indicates a merge-train run.
    ///
    /// GitHub merge-group CI runs push to branches named
    /// `gh-readonly-queue/<target>/pr-<number>-<hash>` and report the event as
    /// `merge_group`.
    pub fn detect_merge_train(branch: &str, event: &str) -> bool {
        branch.starts_with("gh-readonly-queue/") || event.eq_ignore_ascii_case("merge_group")
    }

    /// Build a new trigger, auto-detecting merge-train membership.
    pub fn new(
        branch: impl Into<String>,
        event: impl Into<String>,
        pr_number: Option<u64>,
    ) -> Self {
        let branch = branch.into();
        let event = event.into();
        let is_merge_train = Self::detect_merge_train(&branch, &event);
        Self {
            branch,
            pr_number,
            event,
            is_merge_train,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_train_detected_by_branch_prefix() {
        let trigger = CiPipelineTrigger::new(
            "gh-readonly-queue/main/pr-123-deadbeef",
            "pull_request",
            Some(123),
        );
        assert!(trigger.is_merge_train);
    }

    #[test]
    fn merge_train_detected_by_event() {
        let trigger = CiPipelineTrigger::new("main", "merge_group", None);
        assert!(trigger.is_merge_train);
    }

    #[test]
    fn regular_pr_not_merge_train() {
        let trigger = CiPipelineTrigger::new("feature/foo", "pull_request", Some(42));
        assert!(!trigger.is_merge_train);
    }

    #[test]
    fn plain_push_not_merge_train() {
        let trigger = CiPipelineTrigger::new("main", "push", None);
        assert!(!trigger.is_merge_train);
    }

    #[test]
    fn serde_roundtrip_preserves_flag() {
        let original =
            CiPipelineTrigger::new("gh-readonly-queue/main/pr-7", "merge_group", Some(7));
        let json = serde_json::to_string(&original).unwrap();
        let roundtripped: CiPipelineTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(original, roundtripped);
        assert!(roundtripped.is_merge_train);
    }
}
