//! Periodic fleet-status digest posted to the operator's Telegram chat.
//!
//! Distinct from [`crate::telegram_reply_poller`] (which drains `getUpdates`
//! and is gated to a single node because Telegram allows only one long-poll
//! holder per bot token): `sendMessage` has no such restriction, so this tick
//! can run on every daemon. It leader-gates itself via
//! [`crate::leader_cache::is_current_leader`] so only one digest goes out
//! per interval instead of one per fleet member.

use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Spawn the telegram status-updater tick. A no-op tick (not leader, or
/// telegram not configured in `fleet_secrets`) is silent — see
/// [`crate::telegram::send_telegram_from_secrets`].
pub fn spawn_telegram_status_updater_tick(
    pg: PgPool,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }

                    match fetch_worker_counts(&pg).await {
                        Ok((online, total)) => {
                            let digest = format_status_digest(online, total);
                            if let Err(err) =
                                crate::telegram::send_telegram_from_secrets(&pg, "ForgeFleet status", &digest)
                                    .await
                            {
                                warn!(error = %err, "telegram status updater: send failed");
                            }
                        }
                        Err(err) => warn!(error = %err, "telegram status updater: fleet_workers query failed"),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("telegram status updater shutting down");
                        break;
                    }
                }
            }
        }
    })
}

async fn fetch_worker_counts(pg: &PgPool) -> Result<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status = 'online'), COUNT(*) FROM fleet_workers",
    )
    .fetch_one(pg)
    .await?;
    Ok(row)
}

fn format_status_digest(online: i64, total: i64) -> String {
    format!("{online}/{total} fleet nodes online")
}

#[cfg(test)]
mod tests {
    use super::format_status_digest;

    #[test]
    fn format_status_digest_reports_online_ratio() {
        assert_eq!(format_status_digest(3, 5), "3/5 fleet nodes online");
    }
}
