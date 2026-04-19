//! Best-effort NATS publisher for Pulse.
//!
//! Mirrors pulse beats to NATS subject `fleet.pulse.{computer_name}` so
//! dashboards and log aggregators can react in real time without
//! long-poll. NATS is optional: if the connection fails at startup, all
//! publish calls silently no-op and Pulse keeps working on Redis alone.
//!
//! URL is resolved from `FORGEFLEET_NATS_URL`, defaulting to
//! `nats://127.0.0.1:4222`.

use async_nats::Client;
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::beat_v2::PulseBeatV2;

static NATS: OnceCell<Option<Client>> = OnceCell::const_new();

/// Default NATS URL.
pub const DEFAULT_NATS_URL: &str = "nats://127.0.0.1:4222";

/// Resolve the NATS URL via `FORGEFLEET_NATS_URL`, defaulting locally.
pub fn resolve_nats_url() -> String {
    std::env::var("FORGEFLEET_NATS_URL").unwrap_or_else(|_| DEFAULT_NATS_URL.to_string())
}

/// Lazily connect to NATS on first call. Returns `Some(&Client)` on
/// success, `None` if the connection failed — callers must tolerate
/// the `None` case (NATS is optional infrastructure).
pub async fn get_or_init_nats() -> Option<&'static Client> {
    let slot = NATS
        .get_or_init(|| async {
            let url = resolve_nats_url();
            match async_nats::connect(&url).await {
                Ok(c) => {
                    debug!(url = %url, "ff-pulse: NATS connected");
                    Some(c)
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "ff-pulse: NATS unavailable — pulse will only flow via Redis");
                    None
                }
            }
        })
        .await;
    slot.as_ref()
}

/// Publish a pulse beat to NATS `fleet.pulse.{computer_name}`. Best-effort,
/// never errors the caller — any serialization or transport failure is
/// swallowed.
pub async fn publish_pulse_beat(computer_name: &str, beat: &PulseBeatV2) {
    let Some(client) = get_or_init_nats().await else {
        return;
    };
    let subject = format!("fleet.pulse.{}", computer_name);
    let bytes = match serde_json::to_vec(beat) {
        Ok(b) => b,
        Err(_) => return,
    };
    if let Err(e) = client.publish(subject.clone(), bytes.into()).await {
        debug!(%subject, error = %e, "ff-pulse: NATS publish failed (non-fatal)");
    }
}

/// Publish a fleet member status transition (e.g. online → offline).
/// The materializer detects these from incoming pulse beats and fans them
/// out to `fleet.events.member.{name}.{online|offline}`.
pub async fn publish_member_status_transition(name: &str, prev: &str, new: &str) {
    let Some(client) = get_or_init_nats().await else {
        return;
    };
    // Map DB status strings to the two top-level states we publish.
    let bucket = match new {
        "online" => "online",
        // "offline", "odown", "sdown" all surface as offline for subscribers.
        _ => "offline",
    };
    let subject = format!("fleet.events.member.{name}.{bucket}");
    let payload = serde_json::json!({
        "name": name,
        "prev": prev,
        "new": new,
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    let bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(_) => return,
    };
    if let Err(e) = client.publish(subject.clone(), bytes.into()).await {
        debug!(%subject, error = %e, "ff-pulse: NATS publish failed (non-fatal)");
    }
}
