//! In-process tracker for error signatures observed by ff-agent.
//!
//! Recurring signatures are *auto-recycled* (ignored) so that only *novel*
//! signatures generate a Telegram alert. The tracker is intentionally thin:
//! persistent single-flight is handled by the DB (`ON CONFLICT DO NOTHING` on
//! `fleet_tasks.dedup_signature`), while this module avoids duplicate Telegram
//! noise within a single process lifetime.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use sqlx::PgPool;

use crate::telegram::send_telegram_from_secrets;

/// Default TTL for an observed signature. Chosen to match the interaction-error
/// scan lookback so a recurring error is still considered "seen" at the next
/// scan window.
const DEFAULT_SIGNATURE_TTL: Duration = Duration::from_secs(35 * 60);

/// Tracks recently seen error signatures and fires Telegram alerts for novel
/// ones.
#[derive(Debug, Clone)]
pub struct ErrorTracker {
    seen: Arc<DashMap<String, Instant>>,
    ttl: Duration,
}

impl ErrorTracker {
    /// Create a tracker that recycles signatures for `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: Arc::new(DashMap::new()),
            ttl,
        }
    }

    /// Mark a signature as observed. Returns `true` when the signature is novel
    /// (not seen within the TTL), `false` when it has been auto-recycled.
    pub fn observe(&self, signature: &str) -> bool {
        let now = Instant::now();
        match self.seen.entry(signature.to_string()) {
            dashmap::Entry::Occupied(mut entry) => {
                if now.duration_since(*entry.get()) < self.ttl {
                    false
                } else {
                    entry.insert(now);
                    true
                }
            }
            dashmap::Entry::Vacant(entry) => {
                entry.insert(now);
                true
            }
        }
    }

    /// Send a Telegram alert for a novel error signature.
    ///
    /// The alert includes the signature and the full error message. Any failure
    /// is logged and swallowed so alerting can never break the caller's flow.
    pub async fn alert(&self, pg: &PgPool, signature: &str, error_text: Option<&str>) {
        if !self.observe(signature) {
            tracing::debug!(
                error_signature = %signature,
                "error_tracker: auto-recycled signature; skipping telegram alert"
            );
            return;
        }

        let (title, body) = format_alert(signature, error_text);
        match send_telegram_from_secrets(pg, &title, &body).await {
            Ok(()) => {
                tracing::info!(
                    error_signature = %signature,
                    "error_tracker: telegram alert sent for novel error signature"
                );
            }
            Err(err) => {
                tracing::warn!(
                    error_signature = %signature,
                    error = %err,
                    "error_tracker: telegram alert failed"
                );
            }
        }
    }
}

impl Default for ErrorTracker {
    fn default() -> Self {
        Self::new(DEFAULT_SIGNATURE_TTL)
    }
}

/// Build the Telegram alert title and body for a novel error signature.
pub fn format_alert(signature: &str, error_text: Option<&str>) -> (String, String) {
    let title = "🚨 ForgeFleet novel error".to_string();
    let error_text = error_text
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("<no error text>");
    let body = format!("Signature: {signature}\n\nFull error:\n{error_text}");
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_reports_novel_then_recycles() {
        let tracker = ErrorTracker::new(Duration::from_secs(60));
        assert!(tracker.observe("sig-1"));
        assert!(!tracker.observe("sig-1"));
        assert!(tracker.observe("sig-2"));
    }

    #[test]
    fn observe_novel_after_ttl_expires() {
        let tracker = ErrorTracker::new(Duration::from_millis(1));
        assert!(tracker.observe("sig"));
        std::thread::sleep(Duration::from_millis(10));
        assert!(tracker.observe("sig"));
    }

    #[test]
    fn format_alert_contains_signature_and_full_error() {
        let (title, body) = format_alert("sig-abc", Some("something went wrong"));
        assert!(title.contains("novel error"));
        assert!(body.contains("sig-abc"));
        assert!(body.contains("something went wrong"));
    }

    #[test]
    fn format_alert_falls_back_when_error_text_missing_or_empty() {
        let (_, body) = format_alert("sig", None);
        assert!(body.contains("sig"));
        assert!(body.contains("<no error text>"));

        let (_, body) = format_alert("sig", Some("   "));
        assert!(body.contains("<no error text>"));
    }
}
