//! Work-item dispatch policy for the control plane.
//!
//! Duration knobs for a dispatched work_item build. The two windows are
//! deliberately distinct: `lease_duration` bounds how long a lease may go
//! WITHOUT a heartbeat before the reaper re-queues the item (a dead slot),
//! while `max_build_duration` bounds total build wall-clock even when
//! heartbeats stay fresh (the "building forever, live heartbeat" wedge).

use std::time::Duration;

/// Default lease lifetime granted at dispatch, refreshed by heartbeats. The
/// heartbeat reaper reclaims the lease and re-queues the work_item once it
/// goes this long without a beat.
pub const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(600);

/// Default ceiling on a single build's wall-clock time. Enforced regardless
/// of heartbeat freshness, so a wedged build that keeps beating still gets
/// cut off.
pub const DEFAULT_MAX_BUILD_DURATION: Duration = Duration::from_secs(300);

/// Per-dispatch execution policy for a work_item build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemDispatch {
    /// Lease lifetime between heartbeats; the reaper re-queues the item once
    /// the lease goes this long unbeaten.
    pub lease_duration: Duration,
    /// Max build wall-clock; a build past this is stopped even with a fresh
    /// heartbeat.
    pub max_build_duration: Duration,
}

impl WorkItemDispatch {
    pub fn new() -> Self {
        Self {
            lease_duration: DEFAULT_LEASE_DURATION,
            max_build_duration: DEFAULT_MAX_BUILD_DURATION,
        }
    }

    pub fn with_lease_duration(mut self, lease_duration: Duration) -> Self {
        self.lease_duration = lease_duration;
        self
    }

    pub fn with_max_build_duration(mut self, max_build_duration: Duration) -> Self {
        self.max_build_duration = max_build_duration;
        self
    }
}

impl Default for WorkItemDispatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_build_duration_defaults_to_300_seconds() {
        let dispatch = WorkItemDispatch::default();
        assert_eq!(dispatch.max_build_duration, Duration::from_secs(300));
    }

    #[test]
    fn lease_duration_defaults_mirror_heartbeat_reaping_grant() {
        let dispatch = WorkItemDispatch::default();
        assert_eq!(dispatch.lease_duration, DEFAULT_LEASE_DURATION);
    }

    #[test]
    fn builders_override_defaults() {
        let dispatch = WorkItemDispatch::new()
            .with_lease_duration(Duration::from_secs(120))
            .with_max_build_duration(Duration::from_secs(900));
        assert_eq!(dispatch.lease_duration, Duration::from_secs(120));
        assert_eq!(dispatch.max_build_duration, Duration::from_secs(900));
    }
}
