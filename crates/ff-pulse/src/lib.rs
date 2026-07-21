//! Fleet Pulse — real-time fleet metrics via Redis.
//!
//! Tier 1 of the three-tier storage architecture:
//! - Tier 1: Redis (Fleet Pulse — real-time, ephemeral)
//! - Tier 2: Postgres (durable, queryable)
//! - Tier 3: Git (distributed archive)
//!
//! Each fleet node publishes heartbeats containing system metrics to Redis
//! with a 30-second TTL. If a key expires, the node is considered offline.
//! The dashboard subscribes to pub/sub channels for real-time updates.

// -----------------------------------------------------------------------------
// Schema version tracking
// -----------------------------------------------------------------------------

/// Current Pulse beat schema version, published on the wire as
/// [`beat_v2::PulseBeatV2::pulse_protocol_version`].
///
/// Version bump logic: bump this by exactly one whenever a BREAKING change
/// lands in the beat payload (removing/renaming a field, or changing a field's
/// type or meaning). Purely additive fields with `#[serde(default)]` do NOT
/// need a bump. Bumping this constant is the whole procedure — the wire value
/// in [`beat_v2::PulseBeatV2::skeleton`] and the compatibility window below
/// both derive from it.
pub const PULSE_SCHEMA_VERSION: u32 = 2;

/// How many generations of older publishers a reader must keep accepting.
/// This is the one-generation backward-compatibility rule: consumers built
/// against version N must still parse beats from version N-1, so the fleet
/// can upgrade node-by-node without dropping heartbeats.
pub const SCHEMA_COMPAT_GENERATIONS: u32 = 1;

/// Oldest schema version this build still accepts. Derived from
/// [`PULSE_SCHEMA_VERSION`], so bumping the current version automatically
/// shifts the window forward and drops support for version N-2.
pub const MIN_COMPATIBLE_SCHEMA_VERSION: u32 =
    PULSE_SCHEMA_VERSION.saturating_sub(SCHEMA_COMPAT_GENERATIONS);

/// True when a beat published at `version` may be consumed by this build:
/// current, or at most [`SCHEMA_COMPAT_GENERATIONS`] behind. Versions newer
/// than this build (a breaking change this node hasn't deployed yet) are
/// rejected — the reader has no idea how to interpret them.
pub const fn is_schema_compatible(version: u32) -> bool {
    version >= MIN_COMPATIBLE_SCHEMA_VERSION && version <= PULSE_SCHEMA_VERSION
}

pub mod beat_v2;
pub mod client;
pub mod cx7_detect;
pub mod detection_registry;
pub mod docker_probe;
pub mod error;
pub mod fabric_upsert;
pub mod heartbeat;
pub mod heartbeat_v2;
pub mod llm_probe;
pub mod materializer;
pub mod metrics;
pub mod mlx_adapter;
pub mod nats;
pub mod peer_map;
pub mod pulse_hmac;
pub mod ray_detect;
pub mod reader;
pub mod slm_monitor;
pub mod software_collector;
pub mod subscriber;
pub mod worker;

pub use worker::Worker;

pub use client::PulseClient;
pub use error::{PulseError, Result};
pub use heartbeat::HeartbeatPublisher;
pub use heartbeat_v2::HeartbeatV2Publisher;
pub use metrics::{FleetSnapshot, LoadedModel, NodeMetrics, PulseEvent, PulseEventType};
pub use software_collector::SoftwareCollector;
pub use subscriber::PulseSubscriber;

#[cfg(test)]
mod schema_version_tests {
    use super::*;

    #[test]
    fn compat_window_is_exactly_one_generation() {
        assert_eq!(SCHEMA_COMPAT_GENERATIONS, 1);
        assert_eq!(
            MIN_COMPATIBLE_SCHEMA_VERSION,
            PULSE_SCHEMA_VERSION - SCHEMA_COMPAT_GENERATIONS
        );
    }

    #[test]
    fn current_and_previous_generation_are_compatible() {
        assert!(is_schema_compatible(PULSE_SCHEMA_VERSION));
        assert!(is_schema_compatible(MIN_COMPATIBLE_SCHEMA_VERSION));
    }

    #[test]
    fn versions_outside_the_window_are_rejected() {
        assert!(!is_schema_compatible(PULSE_SCHEMA_VERSION + 1));
        if MIN_COMPATIBLE_SCHEMA_VERSION > 0 {
            assert!(!is_schema_compatible(MIN_COMPATIBLE_SCHEMA_VERSION - 1));
        }
    }

    #[test]
    fn skeleton_publishes_current_schema_version() {
        let beat = beat_v2::PulseBeatV2::skeleton("test-node");
        assert_eq!(beat.pulse_protocol_version, PULSE_SCHEMA_VERSION);
        assert!(is_schema_compatible(beat.pulse_protocol_version));
    }
}
