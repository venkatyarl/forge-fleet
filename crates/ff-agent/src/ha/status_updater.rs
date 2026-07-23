//! Periodic Telegram status updater for a long-running session.
//!
//! Wraps [`crate::telegram::send_telegram_recorded`] behind a fixed-interval
//! loop so any session with an id (agent loop run, work-item build, etc.) can
//! broadcast an operator-visible status line to Telegram on a cadence. Reuses
//! the same `telegram_messages` recording as the nightly digest
//! ([`crate::ha::periodic`]), so an operator REPLY to a status update routes
//! back to the session that sent it.
//!
//! Wired into [`crate::work_item_dispatch::dispatch_one`], which is the
//! codebase's existing notion of a live "session": the guard's lifetime spans
//! the whole dispatch (checkout → build → commit → push → PR), matching the
//! `session_id` already stamped on `work_item_leases` and used by the
//! task-failed alert in the same module.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default cadence between status CHECKS. A check only becomes a Telegram
/// send when the status line changed (or the unchanged-heartbeat is due) —
/// see [`should_send`].
pub const DEFAULT_UPDATE_INTERVAL: Duration = Duration::from_secs(300);

/// Re-send cadence for an UNCHANGED status line. With one updater per
/// concurrent build (~25 fleet-wide), sending every tick regardless of change
/// flooded the operator with dozens of identical "building X on Y" messages
/// per hour (operator-reported 2026-07-22). An unchanged status now re-sends
/// only this often, as a liveness heartbeat; transitions send immediately on
/// the next tick.
pub const UNCHANGED_RESEND_INTERVAL: Duration = Duration::from_secs(1800);

/// Pure send decision: send when the status line CHANGED, or when the
/// unchanged heartbeat is due. Split out for unit testing.
pub fn should_send(changed: bool, since_last_send: Duration) -> bool {
    changed || since_last_send >= UNCHANGED_RESEND_INTERVAL
}

/// Supplies the current status line for a session at send time. Implemented
/// as a trait (with a blanket impl for `Fn() -> String`) so this module stays
/// decoupled from whatever tracks the caller's live session state.
pub trait SessionStatusSource: Send + Sync {
    fn status(&self) -> String;
}

impl<F> SessionStatusSource for F
where
    F: Fn() -> String + Send + Sync,
{
    fn status(&self) -> String {
        (self)()
    }
}

/// Telegram title for a session status update. The session id is always
/// included so the operator can tell sessions apart at a glance and so a
/// reply can be traced back to it.
pub fn format_title(session_id: &str) -> String {
    format!("Session {session_id} status")
}

/// Periodically sends the current status of one session to Telegram.
pub struct SessionStatusUpdater {
    pg: PgPool,
    session_id: String,
    source: Arc<dyn SessionStatusSource>,
    interval: Duration,
}

impl SessionStatusUpdater {
    /// Create an updater for `session_id`, using [`DEFAULT_UPDATE_INTERVAL`].
    /// `session_id` must be the caller's real session identifier — never a
    /// placeholder — since it is both shown to the operator and used to key
    /// the `telegram_messages` row that routes replies back to the session.
    pub fn new(
        pg: PgPool,
        session_id: impl Into<String>,
        source: Arc<dyn SessionStatusSource>,
    ) -> Self {
        Self {
            pg,
            session_id: session_id.into(),
            source,
            interval: DEFAULT_UPDATE_INTERVAL,
        }
    }

    /// Override the default send cadence.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Send one status update now. Returns the Telegram message id when a
    /// message was actually sent, `None` when Telegram isn't configured
    /// (missing bot token / chat id secrets — not treated as an error).
    pub async fn send_once(&self) -> Result<Option<i64>> {
        let body = self.source.status();
        self.send_body(&body).await
    }

    async fn send_body(&self, body: &str) -> Result<Option<i64>> {
        let title = format_title(&self.session_id);
        crate::telegram::send_telegram_recorded(&self.pg, &title, body, &self.session_id).await
    }

    /// Spawn the periodic send loop. Runs until `shutdown` is set to `true`
    /// or its sender is dropped. Each tick CHECKS the status; it only SENDS on
    /// a transition (status line changed) or when the unchanged-status
    /// heartbeat ([`UNCHANGED_RESEND_INTERVAL`]) is due — identical statuses
    /// must not flood the operator ([`should_send`]).
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Treat the spawn-time status as already-sent WITHOUT sending it:
            // tokio's interval fires its first tick IMMEDIATELY, so seeding
            // None here meant every dispatch attempt fired one "building…"
            // message at spawn — and a fast-fail retry loop (dispatch dies in
            // seconds, item requeues, new dispatch, new guard) turned that
            // into a per-minute per-item flood (operator-reported 2026-07-23,
            // second spam wave). The operator-facing "what's building" view is
            // the batched 10-minute digest; this per-session channel only
            // speaks on a genuine status TRANSITION or the 30-minute
            // long-build heartbeat.
            let mut last_sent_body: Option<String> = Some(self.source.status());
            let mut last_sent_at = tokio::time::Instant::now();

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let body = self.source.status();
                        let changed = last_sent_body.as_deref() != Some(body.as_str());
                        if !should_send(changed, last_sent_at.elapsed().into()) {
                            debug!(
                                session_id = %self.session_id,
                                "session status updater: unchanged — skipping"
                            );
                            continue;
                        }
                        match self.send_body(&body).await {
                            Ok(Some(message_id)) => {
                                last_sent_body = Some(body);
                                last_sent_at = tokio::time::Instant::now();
                                info!(
                                    session_id = %self.session_id,
                                    tg_message_id = message_id,
                                    "session status updater: sent"
                                )
                            }
                            Ok(None) => debug!(
                                session_id = %self.session_id,
                                "session status updater: telegram not configured; skipping"
                            ),
                            Err(e) => warn!(
                                session_id = %self.session_id,
                                error = %e,
                                "session status updater: send failed"
                            ),
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!(session_id = %self.session_id, "session status updater loop stopped");
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// RAII guard pairing a [`SessionStatusUpdater::spawn`] loop with the scope
/// it should live for: stops the loop as soon as the guard drops, on any exit
/// path (success, early return, or error), mirroring
/// [`crate::work_item_dispatch::HeartbeatGuard`].
pub struct SessionStatusGuard {
    stop_tx: watch::Sender<bool>,
}

impl SessionStatusGuard {
    pub fn spawn(
        pg: PgPool,
        session_id: impl Into<String>,
        source: Arc<dyn SessionStatusSource>,
    ) -> Self {
        let (stop_tx, stop_rx) = watch::channel(false);
        drop(SessionStatusUpdater::new(pg, session_id, source).spawn(stop_rx));
        Self { stop_tx }
    }
}

impl Drop for SessionStatusGuard {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_title_includes_session_id() {
        assert_eq!(
            format_title("f47ac10b-58cc-4372-a567-0e02b2c3d479"),
            "Session f47ac10b-58cc-4372-a567-0e02b2c3d479 status"
        );
    }

    #[test]
    fn closure_source_reports_status() {
        let source: Arc<dyn SessionStatusSource> = Arc::new(|| "building".to_string());
        assert_eq!(source.status(), "building");
    }

    #[test]
    fn unchanged_status_is_suppressed_until_heartbeat() {
        // A transition always sends.
        assert!(should_send(true, Duration::from_secs(0)));
        // Unchanged: silent within the heartbeat window…
        assert!(!should_send(false, Duration::from_secs(0)));
        assert!(!should_send(
            false,
            UNCHANGED_RESEND_INTERVAL - Duration::from_secs(1)
        ));
        // …and re-sent once the heartbeat is due.
        assert!(should_send(false, UNCHANGED_RESEND_INTERVAL));
        assert!(should_send(
            false,
            UNCHANGED_RESEND_INTERVAL + Duration::from_secs(60)
        ));
    }
}
