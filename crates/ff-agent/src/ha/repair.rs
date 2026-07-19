//! Repair status tracking for HA sources.
//!
//! Tracks the lifecycle of repair actions, including the `src-offline` edge
//! status used when a source becomes unavailable while a repair is in flight
//! and the `key-changed` edge status used when a source's SSH host key no
//! longer matches known_hosts (operator remediation required before retry).

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
    /// Source's SSH host key changed (known_hosts mismatch). Not retryable
    /// until an operator re-trusts the key.
    #[serde(rename = "key-changed")]
    KeyChanged,
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
                | (RepairStatus::Proposed, RepairStatus::KeyChanged)
                | (RepairStatus::Applying, RepairStatus::KeyChanged)
                | (RepairStatus::KeyChanged, RepairStatus::Applying)
                | (RepairStatus::KeyChanged, RepairStatus::Failed)
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

/// Returns true if `err` is an SSH "host key changed" failure — the key on
/// record in known_hosts no longer matches what the source presents — as
/// opposed to a first-contact verification failure or a connectivity error.
pub fn is_host_key_changed_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("remote host identification has changed")
        || (e.contains("host key for") && e.contains("changed"))
        || (e.contains("offending") && e.contains("known_hosts"))
}

/// Maps an error from a repair attempt to the status the repair should move
/// to. A changed host key gets the dedicated `key-changed` status instead of
/// the retryable `src-offline` bucket: retrying cannot succeed until an
/// operator re-trusts the key, and blind retries would mask a possible
/// man-in-the-middle.
pub fn repair_status_for_error(err: &str) -> RepairStatus {
    if is_host_key_changed_error(err) {
        return RepairStatus::KeyChanged;
    }
    let e = err.to_ascii_lowercase();
    if e.contains("connection refused")
        || e.contains("connection timed out")
        || e.contains("no route to host")
        || e.contains("ssh: connect to host")
        || e.contains("could not resolve hostname")
    {
        return RepairStatus::SrcOffline;
    }
    RepairStatus::Failed
}

/// Operator instructions for clearing a `key-changed` repair status.
pub fn key_changed_remediation(host: &str) -> String {
    format!(
        "SSH host key for '{host}' changed. If the change is expected \
         (reinstall or key rotation): 1) remove the stale entry with \
         `ssh-keygen -R {host}`; 2) re-trust the new key with \
         `ssh-keyscan -H {host} >> ~/.ssh/known_hosts`; 3) retry the repair. \
         If the change is NOT expected, do not reconnect — investigate a \
         possible man-in-the-middle before trusting the new key."
    )
}

/// Status reported for a repair action, with remediation instructions when
/// operator action is required before the repair can proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairStatusResponse {
    pub status: RepairStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

impl RepairStatusResponse {
    /// Builds the status response for an error from a repair attempt against
    /// `host`, attaching SSH key remediation instructions when the host key
    /// changed.
    pub fn from_repair_error(host: &str, err: &str) -> Self {
        let status = repair_status_for_error(err);
        let remediation =
            (status == RepairStatus::KeyChanged).then(|| key_changed_remediation(host));
        Self {
            status,
            remediation,
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

    #[test]
    fn key_changed_is_reachable_from_proposed_and_applying() {
        assert!(RepairStatus::Proposed.can_transition_to(RepairStatus::KeyChanged));
        assert!(RepairStatus::Applying.can_transition_to(RepairStatus::KeyChanged));
    }

    #[test]
    fn key_changed_can_resume_or_fail() {
        assert!(RepairStatus::KeyChanged.can_transition_to(RepairStatus::Applying));
        assert!(RepairStatus::KeyChanged.can_transition_to(RepairStatus::Failed));
    }

    #[test]
    fn key_changed_cannot_skip_to_terminal_success() {
        assert!(!RepairStatus::KeyChanged.can_transition_to(RepairStatus::Verified));
        assert!(!RepairStatus::KeyChanged.can_transition_to(RepairStatus::RolledBack));
    }

    #[test]
    fn serde_round_trips_key_changed() {
        assert_eq!(
            serde_json::to_value(RepairStatus::KeyChanged).unwrap(),
            serde_json::json!("key-changed")
        );
        let status: RepairStatus = serde_json::from_str("\"key-changed\"").unwrap();
        assert_eq!(status, RepairStatus::KeyChanged);
    }

    #[test]
    fn detects_host_key_changed_errors() {
        assert!(is_host_key_changed_error(
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@"
        ));
        assert!(is_host_key_changed_error(
            "The ECDSA host key for 192.168.5.108 has changed"
        ));
        assert!(is_host_key_changed_error(
            "Offending ED25519 key in /home/ff/.ssh/known_hosts:12"
        ));
        // First-contact verification failure is NOT a changed key.
        assert!(!is_host_key_changed_error("Host key verification failed."));
        assert!(!is_host_key_changed_error(
            "ssh: connect to host 192.168.5.108 port 22: Connection refused"
        ));
    }

    #[test]
    fn key_changed_error_maps_to_key_changed_not_retry() {
        assert_eq!(
            repair_status_for_error(
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\n\
                 Offending ED25519 key in /home/ff/.ssh/known_hosts:12\n\
                 Host key verification failed."
            ),
            RepairStatus::KeyChanged
        );
        assert_eq!(
            repair_status_for_error("ssh: connect to host node-3 port 22: Connection refused"),
            RepairStatus::SrcOffline
        );
        assert_eq!(
            repair_status_for_error("rsync error: some files could not be transferred"),
            RepairStatus::Failed
        );
    }

    #[test]
    fn key_changed_response_carries_remediation() {
        let resp = RepairStatusResponse::from_repair_error(
            "node-3",
            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
        );
        assert_eq!(resp.status, RepairStatus::KeyChanged);
        let remediation = resp.remediation.as_deref().unwrap();
        assert!(remediation.contains("ssh-keygen -R node-3"));
        assert!(remediation.contains("ssh-keyscan -H node-3"));
        assert!(remediation.contains("retry the repair"));
    }

    #[test]
    fn non_key_changed_response_has_no_remediation() {
        let resp = RepairStatusResponse::from_repair_error(
            "node-3",
            "ssh: connect to host node-3 port 22: Connection refused",
        );
        assert_eq!(resp.status, RepairStatus::SrcOffline);
        assert!(resp.remediation.is_none());
        // And remediation is omitted from the serialized response entirely.
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json, serde_json::json!({"status": "src-offline"}));
    }
}
