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

pub mod beat_v2;
pub mod client;
pub mod error;
pub mod heartbeat;
pub mod heartbeat_v2;
pub mod materializer;
pub mod metrics;
pub mod peer_map;
pub mod reader;
pub mod software_collector;
pub mod subscriber;

pub use client::PulseClient;
pub use error::{PulseError, Result};
pub use heartbeat::HeartbeatPublisher;
pub use heartbeat_v2::HeartbeatV2Publisher;
pub use metrics::{FleetSnapshot, LoadedModel, NodeMetrics, PulseEvent, PulseEventType};
pub use software_collector::SoftwareCollector;
pub use subscriber::PulseSubscriber;
