//! NATS log forwarding layer — STUB.
//!
//! TODO (Phase 10 Part 5 follow-up): wire up `async-nats` so each tracing
//! event is published to subject `fleet.logs.{computer}.{service}.{level}`.
//! For now this module only holds the shape of the future API so wiring in
//! `main.rs` can be added incrementally without forcing the `async-nats`
//! dependency on every build.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NatsError {
    #[error("nats not yet wired up: {0}")]
    NotImplemented(&'static str),
}

/// Placeholder for the future NATS-backed tracing layer. Construct with
/// [`NatsLogLayer::new`] — currently always returns
/// [`NatsError::NotImplemented`] so callers can opt-in by setting
/// `FORGEFLEET_NATS_URL`, but the daemon silently skips wiring when not
/// available.
pub struct NatsLogLayer {
    pub computer_name: String,
    pub service: String,
}

impl NatsLogLayer {
    /// Build a new layer. Always returns `Err` until the real wire-up lands.
    pub async fn new(
        _nats_url: &str,
        computer_name: String,
        service: String,
    ) -> Result<Self, NatsError> {
        let _ = Self {
            computer_name,
            service,
        };
        Err(NatsError::NotImplemented(
            "NATS log forwarding not yet implemented — Part 5 stub",
        ))
    }
}
