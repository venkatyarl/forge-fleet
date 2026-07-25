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

                    match build_status_digest(&pg).await {
                        Ok(digest) => {
                            if let Err(err) =
                                crate::telegram::send_telegram_from_secrets(&pg, "🚀 ForgeFleet", &digest)
                                    .await
                            {
                                warn!(error = %err, "telegram status updater: send failed");
                            }
                        }
                        Err(err) => warn!(error = %err, "telegram status updater: digest query failed"),
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

/// Build the full operator digest from the live DB (native ff — replaces the
/// shell band-aid digest, 2026-07-25). Sections, spaced: what's building (with
/// duration/heartbeat/eta + STUCK flag), backlog counts (ready/failed/verified),
/// blocked-on-operator, last rolling deployment. Everything is queried, so it
/// cannot show fake progress.
async fn build_status_digest(pg: &PgPool) -> Result<String> {
    // building items with duration + heartbeat + eta + stuck flag
    let building: Vec<(String, i32, Option<i32>)> = sqlx::query_as(
        "SELECT left(w.title, 34), \
                (EXTRACT(EPOCH FROM (now() - l.created_at)) / 60)::int, \
                (EXTRACT(EPOCH FROM (now() - l.heartbeat_at)))::int \
           FROM work_item_leases l JOIN work_items w ON w.id = l.work_item_id \
          WHERE l.released_at IS NULL ORDER BY l.created_at",
    )
    .fetch_all(pg)
    .await
    .unwrap_or_default();

    // backlog + failure + operator-blocked counts
    let (ready, failed, verified, blocked_op): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status='ready'), \
                COUNT(*) FILTER (WHERE status='failed'), \
                COUNT(*) FILTER (WHERE verified=1), \
                COUNT(*) FILTER (WHERE status='blocked' AND coalesce(last_error,'') ILIKE '%operator%') \
           FROM work_items",
    )
    .fetch_one(pg)
    .await
    .unwrap_or((0, 0, 0, 0));

    // last rolling deployment (best-effort — table may not exist on older DBs)
    let deploy: Option<(String, i32, i32)> = sqlx::query_as(
        "SELECT commit_sha, nodes_updated, nodes_total FROM fleet_deploy_events \
          ORDER BY deployed_at DESC LIMIT 1",
    )
    .fetch_optional(pg)
    .await
    .ok()
    .flatten();

    let mut msg = String::from("🚀 ForgeFleet status\n\n");
    msg.push_str("🔨 Building now (duration · heartbeat · eta):\n");
    if building.is_empty() {
        msg.push_str("• (idle)\n");
    } else {
        for (title, mins, hb) in &building {
            let stuck = hb.map(|h| h > 300).unwrap_or(false);
            let eta = (15 - mins).max(1);
            let hbs = hb.map(|h| h.to_string()).unwrap_or_else(|| "?".into());
            msg.push_str(&format!(
                "• {}{} — {}m in, hb {}s (eta~{}m)\n",
                if stuck { "⚠STUCK " } else { "" },
                title,
                mins,
                hbs,
                eta
            ));
        }
    }
    msg.push('\n');
    if let Some((sha, up, tot)) = deploy {
        msg.push_str(&format!("📦 Rolling deployment: {sha} · {up}/{tot} nodes\n\n"));
    }
    msg.push_str(&format!(
        "📊 ready={ready}  failed={failed}  verified={verified}  ⛔blocked-on-you={blocked_op}"
    ));
    Ok(msg)
}
