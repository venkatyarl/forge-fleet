//! Repair status tracking for HA sources.
//!
//! Tracks the lifecycle of repair actions, including the `src-offline` edge
//! status used when a source becomes unavailable while a repair is in flight,
//! and operator-facing alerts ([`RepairAlert`]) that attach distinct
//! remediation instructions to a repair's status display.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use thiserror::Error;

use ff_pulse::beat_v2::PulseBeatV2;
use ff_pulse::client::PulseClient;
use ff_pulse::reader::PulseReader;

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
    /// The remote host's SSH key changed and the repair is blocked until the
    /// stale known_hosts entry is refreshed.
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
    /// Stable display identifier, matching the serde wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            RepairStatus::Proposed => "proposed",
            RepairStatus::Approved => "approved",
            RepairStatus::Applying => "applying",
            RepairStatus::Applied => "applied",
            RepairStatus::Verified => "verified",
            RepairStatus::Failed => "failed",
            RepairStatus::RolledBack => "rolled_back",
            RepairStatus::Suppressed => "suppressed",
            RepairStatus::SrcOffline => "src-offline",
            RepairStatus::KeyChanged => "key-changed",
        }
    }

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
                | (RepairStatus::Applying, RepairStatus::KeyChanged)
                | (RepairStatus::Applied, RepairStatus::Verified)
                | (RepairStatus::Applied, RepairStatus::Failed)
                | (RepairStatus::Applied, RepairStatus::RolledBack)
                | (RepairStatus::SrcOffline, RepairStatus::Applying)
                | (RepairStatus::SrcOffline, RepairStatus::Failed)
                | (RepairStatus::SrcOffline, RepairStatus::KeyChanged)
                | (RepairStatus::Proposed, RepairStatus::KeyChanged)
                | (RepairStatus::Approved, RepairStatus::KeyChanged)
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

impl fmt::Display for RepairStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of attempting to dispatch a repair edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairDispatch<T> {
    /// The source was online and the repair was dispatched.
    Dispatched(T),
    /// The edge was parked without running the repair. An alert may be attached
    /// so the status response can include remediation instructions.
    Parked(RepairStatus, Option<RepairAlert>),
}

/// Dispatch a repair only while its source node has a live Pulse beat.
///
/// Pulse metrics keys expire when a node stops beating, so this check prevents
/// stale repair edges from being sent to an unavailable source. The dispatch
/// closure is intentionally lazy and is never called for a parked edge.
pub async fn dispatch_repair<F, Fut, T>(
    pulse: &mut PulseClient,
    src_node: &str,
    dispatch: F,
) -> anyhow::Result<RepairDispatch<T>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let src_online = pulse.is_node_alive(src_node).await?;
    dispatch_if_source_online(src_online, src_node, dispatch).await
}

/// Classify a source node's latest Pulse v2 beat as alive or not.
///
/// A source counts as alive only when a beat is present (the 45s beat key
/// TTL has not expired) and the beat is not a graceful-exit (LWT) beat —
/// the same definition `is_odown` and the election use for a healthy peer.
pub fn source_beat_alive(beat: Option<&PulseBeatV2>) -> bool {
    beat.is_some_and(|b| !b.going_offline)
}

/// Dispatch a repair only while its source node's Pulse v2 beat status
/// shows it alive.
///
/// Checks the source's latest beat via [`source_beat_alive`] and returns
/// early with a parked `src-offline` edge when the node has stopped
/// beating (or published its going-offline beat); the dispatch closure is
/// never called in that case.
pub async fn dispatch_repair_by_beat<F, Fut, T>(
    reader: &PulseReader,
    src_node: &str,
    dispatch: F,
) -> anyhow::Result<RepairDispatch<T>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let beat = reader.latest_beat(src_node).await?;
    let src_online = source_beat_alive(beat.as_ref());
    dispatch_if_source_online(src_online, dispatch).await
}

async fn dispatch_if_source_online<F, Fut, T>(
    src_online: bool,
    src_node: &str,
    dispatch: F,
) -> anyhow::Result<RepairDispatch<T>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    if !src_online {
        return Ok(RepairDispatch::Parked(RepairStatus::SrcOffline, None));
    }

    match dispatch().await {
        Ok(v) => Ok(RepairDispatch::Dispatched(v)),
        Err(e) => match classify_repair_error(src_node, &e) {
            Some(alert) => Ok(RepairDispatch::Parked(
                RepairStatus::KeyChanged,
                Some(alert),
            )),
            None => Err(e),
        },
    }
}

/// Inspect a repair failure for a host-key-changed signature. When found,
/// return a [`RepairAlert::KeyChanged`] so callers can surface a distinct
/// `key-changed` status with remediation instructions instead of retrying.
pub fn classify_repair_error(host: &str, err: &anyhow::Error) -> Option<RepairAlert> {
    RepairAlert::from_ssh_output(host, &err.to_string())
}

/// Operator-facing alert attached to a repair that needs manual attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairAlert {
    /// The remote host's SSH key no longer matches the recorded known_hosts
    /// entry — repairs cannot proceed until the stale entry is replaced.
    KeyChanged { host: String },
}

impl RepairAlert {
    /// Classify raw SSH output for `host`, returning a `KeyChanged` alert
    /// when it carries the distinct changed-key signature. A bare
    /// "Host key verification failed." is NOT enough: that also fires for a
    /// merely unknown host, so we require the identification-changed banner
    /// or the "Offending ... key" known_hosts pointer.
    pub fn from_ssh_output(host: &str, output: &str) -> Option<Self> {
        let text = output.to_ascii_lowercase();
        let changed = text.contains("remote host identification has changed")
            || (text.contains("offending") && text.contains("key"));
        changed.then(|| RepairAlert::KeyChanged {
            host: host.to_string(),
        })
    }

    /// Remediation instructions for this alert.
    pub fn remediation(&self) -> String {
        match self {
            RepairAlert::KeyChanged { host } => format!(
                "host key for {host} has changed — verify the change is expected \
                 (reinstall/re-image?), then refresh known_hosts:\n  \
                 ssh-keygen -R {host}\n  \
                 ssh-keyscan -H {host} >> ~/.ssh/known_hosts"
            ),
        }
    }
}

/// Render an operator-facing status line for a repair. With no alert the
/// line is the bare status; a `KeyChanged` alert appends its distinct
/// SSH-keygen remediation instructions.
pub fn display_status(status: RepairStatus, alert: Option<&RepairAlert>) -> String {
    match alert {
        Some(alert) => format!("{status} — {}", alert.remediation()),
        None => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

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
    fn key_changed_detected_from_ssh_banner() {
        let banner = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                      @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!    @\n\
                      Offending ECDSA key in /home/op/.ssh/known_hosts:12\n\
                      Host key verification failed.";
        assert_eq!(
            RepairAlert::from_ssh_output("node-7", banner),
            Some(RepairAlert::KeyChanged {
                host: "node-7".to_string()
            })
        );
    }

    #[test]
    fn key_changed_detected_from_offending_key_line() {
        let out = "Offending RSA key in /root/.ssh/known_hosts:3";
        assert!(RepairAlert::from_ssh_output("node-7", out).is_some());
    }

    #[test]
    fn plain_verification_failure_is_not_key_changed() {
        // Fires for an unknown host too — must not classify as key-changed.
        assert_eq!(
            RepairAlert::from_ssh_output("node-7", "Host key verification failed."),
            None
        );
        assert_eq!(
            RepairAlert::from_ssh_output(
                "node-7",
                "ssh: connect to host node-7: Connection refused"
            ),
            None
        );
    }

    #[test]
    fn key_changed_remediation_has_keygen_commands() {
        let alert = RepairAlert::KeyChanged {
            host: "192.168.5.108".to_string(),
        };
        let r = alert.remediation();
        assert!(r.contains("ssh-keygen -R 192.168.5.108"));
        assert!(r.contains("ssh-keyscan -H 192.168.5.108"));
    }

    #[test]
    fn display_status_is_distinct_for_key_changed() {
        let plain = display_status(RepairStatus::Failed, None);
        assert_eq!(plain, "failed");

        let alert = RepairAlert::KeyChanged {
            host: "node-7".to_string(),
        };
        let with_alert = display_status(RepairStatus::Failed, Some(&alert));
        assert_ne!(with_alert, plain);
        assert!(with_alert.starts_with("failed — "));
        assert!(with_alert.contains("ssh-keygen -R node-7"));
    }

    #[test]
    fn key_changed_alert_serde_round_trip() {
        let alert = RepairAlert::KeyChanged {
            host: "node-7".to_string(),
        };
        let json = serde_json::to_value(&alert).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "key_changed", "host": "node-7"})
        );
        let back: RepairAlert = serde_json::from_value(json).unwrap();
        assert_eq!(back, alert);
    }

    #[test]
    fn missing_beat_is_not_alive() {
        assert!(!source_beat_alive(None));
    }

    #[test]
    fn live_beat_is_alive() {
        let beat = PulseBeatV2::skeleton("node-7");
        assert!(source_beat_alive(Some(&beat)));
    }

    #[test]
    fn going_offline_beat_is_not_alive() {
        let mut beat = PulseBeatV2::skeleton("node-7");
        beat.going_offline = true;
        assert!(!source_beat_alive(Some(&beat)));
    }

    #[tokio::test]
    async fn offline_source_parks_edge_without_dispatching() {
        let called = AtomicBool::new(false);

        let result = dispatch_if_source_online(false, "node-7", || async {
            called.store(true, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(())
        })
        .await
        .unwrap();

        assert_eq!(
            result,
            RepairDispatch::Parked(RepairStatus::SrcOffline, None)
        );
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn online_source_dispatches_repair() {
        let result =
            dispatch_if_source_online(true, "node-7", || async { Ok::<_, anyhow::Error>(42) })
                .await
                .unwrap();

        assert_eq!(result, RepairDispatch::Dispatched(42));
    }

    #[tokio::test]
    async fn host_key_changed_parks_with_key_changed_status_and_alert() {
        let result = dispatch_if_source_online(true, "node-7", || async {
            Err::<(), _>(anyhow::anyhow!(
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                     @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!    @\n\
                     Offending ECDSA key in /home/op/.ssh/known_hosts:12\n\
                     Host key verification failed."
            ))
        })
        .await
        .unwrap();

        assert_eq!(
            result,
            RepairDispatch::Parked(
                RepairStatus::KeyChanged,
                Some(RepairAlert::KeyChanged {
                    host: "node-7".to_string(),
                })
            )
        );
    }

    #[tokio::test]
    async fn non_key_error_is_not_parked() {
        let result = dispatch_if_source_online(true, "node-7", || async {
            Err::<(), _>(anyhow::anyhow!("connection timed out"))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "connection timed out");
    }

    #[test]
    fn key_changed_status_serializes_as_key_changed() {
        assert_eq!(
            serde_json::to_value(RepairStatus::KeyChanged).unwrap(),
            serde_json::json!("key-changed")
        );
        let status: RepairStatus = serde_json::from_str("\"key-changed\"").unwrap();
        assert_eq!(status, RepairStatus::KeyChanged);
    }

    #[test]
    fn key_changed_is_terminal() {
        assert!(!RepairStatus::KeyChanged.can_transition_to(RepairStatus::Applying));
        assert!(!RepairStatus::KeyChanged.can_transition_to(RepairStatus::Verified));
    }

    #[test]
    fn applying_and_src_offline_can_transition_to_key_changed() {
        assert!(RepairStatus::Applying.can_transition_to(RepairStatus::KeyChanged));
        assert!(RepairStatus::SrcOffline.can_transition_to(RepairStatus::KeyChanged));
    }
}
