//! NATS client singleton.
//!
//! A process-global NATS client, initialized once at daemon startup via
//! [`init_nats`]. Callers obtain a reference via [`get_nats`] and publish
//! best-effort — if NATS is unreachable at startup, `get_nats()` returns
//! `None` and all NATS-aware call-sites should fall back to their original
//! Redis/Postgres-only behavior.
//!
//! The URL is read from the `FORGEFLEET_NATS_URL` env var at startup,
//! defaulting to `nats://127.0.0.1:4222`.

use async_nats::Client;
use tokio::sync::OnceCell;

static NATS_CLIENT: OnceCell<Client> = OnceCell::const_new();

/// Default NATS URL when `FORGEFLEET_NATS_URL` is unset.
pub const DEFAULT_NATS_URL: &str = "nats://127.0.0.1:4222";

/// Resolve the NATS URL: env var `FORGEFLEET_NATS_URL` or fallback.
pub fn resolve_nats_url() -> String {
    std::env::var("FORGEFLEET_NATS_URL").unwrap_or_else(|_| DEFAULT_NATS_URL.to_string())
}

/// Fetch the global NATS client if it was successfully initialized.
///
/// Returns `None` if [`init_nats`] was never called or failed. Call-sites
/// should treat a `None` as "NATS not available" and no-op — they must not
/// block or error because NATS is optional.
pub async fn get_nats() -> Option<&'static Client> {
    NATS_CLIENT.get()
}

/// Initialize the global NATS client. Safe to call multiple times; only
/// the first successful call actually installs the client.
///
/// On error, callers should log a warning and proceed — the rest of the
/// daemon must still run on Redis + Postgres alone.
pub async fn init_nats(url: &str) -> Result<(), async_nats::ConnectError> {
    // If already set, short-circuit — ConnectError doesn't have a trivial
    // constructor so we just return Ok.
    if NATS_CLIENT.get().is_some() {
        return Ok(());
    }
    match async_nats::connect(url).await {
        Ok(client) => {
            let _ = NATS_CLIENT.set(client);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Best-effort publish. Serializes `payload` as JSON and fires it at the
/// given subject; drops the result. Never blocks the caller beyond the
/// NATS client's own buffering.
pub async fn publish_json<T: serde::Serialize>(subject: impl Into<String>, payload: &T) {
    let Some(client) = get_nats().await else {
        return;
    };
    let bytes = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(_) => return,
    };
    let _ = client.publish(subject.into(), bytes.into()).await;
}

/// Best-effort publish of raw bytes.
pub async fn publish_raw(subject: impl Into<String>, data: Vec<u8>) {
    let Some(client) = get_nats().await else {
        return;
    };
    let _ = client.publish(subject.into(), data.into()).await;
}
