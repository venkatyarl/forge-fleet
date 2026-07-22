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

/// Default cadence between status updates.
pub const DEFAULT_UPDATE_INTERVAL: Duration = Duration::from_secs(300);

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
        let title = format_title(&self.session_id);
        let body = self.source.status();
        crate::telegram::send_telegram_recorded(&self.pg, &title, &body, &self.session_id).await
    }

    /// Spawn the periodic send loop. Runs until `shutdown` is set to `true`
    /// or its sender is dropped.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match self.send_once().await {
                            Ok(Some(message_id)) => info!(
                                session_id = %self.session_id,
                                tg_message_id = message_id,
                                "session status updater: sent"
                            ),
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
}
