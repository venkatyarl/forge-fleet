//! Repair status tracking for HA sources.
//!
//! Tracks the lifecycle of repair actions, including the `src-offline` edge
//! status used when a source becomes unavailable while a repair is in flight.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Lifecycle status of a repair action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    Proposed,
    Approved,
    Applying,
    Applied,
    Verified,
    Failed,
    RolledBack,
    Suppressed,
    /// Source went offline while the repair was being tracked.
    #[serde(rename = "src-offline")]
    SrcOffline,
}

#[derive(Debug, Error)]
#[error("invalid repair status transition from {from:?} to {to:?}")]
pub struct InvalidRepairTransition {
    pub from: RepairStatus,
    pub to: RepairStatus,
}

impl RepairStatus {
    /// Returns true if `self` can transition to `next`.
    pub fn can_transition_to(self, next: RepairStatus) -> bool {
        matches!(
            (self, next),
            (RepairStatus::Proposed, RepairStatus::Approved)
                | (RepairStatus::Proposed, RepairStatus::Suppressed)
                | (RepairStatus::Proposed, RepairStatus::SrcOffline)
                | (RepairStatus::Approved, RepairStatus::Applying)
                | (RepairStatus::Approved, RepairStatus::Failed)
                | (RepairStatus::Applying, RepairStatus::Applied)
                | (RepairStatus::Applying, RepairStatus::Failed)
                | (RepairStatus::Applying, RepairStatus::SrcOffline)
                | (RepairStatus::Applied, RepairStatus::Verified)
                | (RepairStatus::Applied, RepairStatus::Failed)
                | (RepairStatus::Applied, RepairStatus::RolledBack)
                | (RepairStatus::SrcOffline, RepairStatus::Applying)
                | (RepairStatus::SrcOffline, RepairStatus::Failed)
        )
    }

    /// Validates a transition, returning `Ok(next)` on success.
    pub fn transition(self, next: RepairStatus) -> Result<RepairStatus, InvalidRepairTransition> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(InvalidRepairTransition {
                from: self,
                to: next,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn src_offline_is_reachable_from_proposed_and_applying() {
        assert!(RepairStatus::Proposed.can_transition_to(RepairStatus::SrcOffline));
        assert!(RepairStatus::Applying.can_transition_to(RepairStatus::SrcOffline));
    }

    #[test]
    fn src_offline_can_resume_or_fail() {
        assert!(RepairStatus::SrcOffline.can_transition_to(RepairStatus::Applying));
        assert!(RepairStatus::SrcOffline.can_transition_to(RepairStatus::Failed));
    }

    #[test]
    fn src_offline_cannot_skip_to_terminal_success() {
        assert!(!RepairStatus::SrcOffline.can_transition_to(RepairStatus::Verified));
        assert!(!RepairStatus::SrcOffline.can_transition_to(RepairStatus::RolledBack));
    }

    #[test]
    fn serializes_as_src_offline() {
        assert_eq!(
            serde_json::to_value(RepairStatus::SrcOffline).unwrap(),
            serde_json::json!("src-offline")
        );
    }

    #[test]
    fn deserializes_src_offline() {
        let status: RepairStatus = serde_json::from_str("\"src-offline\"").unwrap();
        assert_eq!(status, RepairStatus::SrcOffline);
    }
}
