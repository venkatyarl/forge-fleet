//! Leader-gated task-history retention.
//!
//! The ephemeral task-history tables (`fleet_tasks`, `deferred_tasks`) accumulate
//! a terminal row per dispatched/deferred task and were never pruned — observed
//! 2026-07-07 at ~45k fleet_tasks + ~18k deferred_tasks, all history. This tick
//! deletes terminal (`completed`/`failed`/`cancelled`) rows older than a
//! retention window so the tables stay bounded. It deliberately does NOT touch
//! the PM `work_items` table (those rows are operator-meaningful).
//!
//! Leader-gated, runs by DEFAULT; opt out with `fleet_secrets.task_retention_mode=off`.
//! Window is `fleet_secrets.task_retention_days` (default 7, floored at 1 so a bad
//! secret can't wipe live history).

use anyhow::Result;
use sqlx::PgPool;
use tracing::{info, warn};

const MODE_KEY: &str = "task_retention_mode";
const RETENTION_DAYS_KEY: &str = "task_retention_days";
const DEFAULT_RETENTION_DAYS: i32 = 7;

/// `off`/`false`/`0`/`disabled`/`no` (case-insensitive) disable the tick; any
/// other value — including a missing secret — leaves it ON.
fn mode_is_off(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("off" | "false" | "0" | "disabled" | "no")
    )
}

/// Parse the retention window, floored at 1 day so a malformed/empty secret can
/// never collapse to 0 and delete live history. Missing → default.
fn retention_days(v: Option<&str>) -> i32 {
    v.and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS)
        .max(1)
}

/// One retention pass. Leader-only; returns `(fleet_tasks_deleted,
/// deferred_tasks_deleted)`.
pub async fn evaluate_retention(pg: &PgPool) -> Result<(u64, u64)> {
    if !crate::leader_cache::is_current_leader() {
        return Ok((0, 0));
    }
    let mode = ff_db::pg_get_secret(pg, MODE_KEY).await.ok().flatten();
    if mode_is_off(mode.as_deref()) {
        return Ok((0, 0));
    }
    let days = retention_days(
        ff_db::pg_get_secret(pg, RETENTION_DAYS_KEY)
            .await
            .ok()
            .flatten()
            .as_deref(),
    );
    let (fleet, deferred) = ff_db::pg_prune_terminal_task_history(pg, days).await?;
    let (raw, hourly, daily) = ff_db::pg_maintain_computer_metrics_history(pg).await?;
    if fleet > 0 || deferred > 0 {
        info!(
            fleet_tasks = fleet,
            deferred_tasks = deferred,
            retention_days = days,
            "task_retention: pruned terminal task history"
        );
    }
    if raw > 0 || hourly > 0 || daily > 0 {
        info!(
            raw,
            hourly, daily, "task_retention: rolled up and pruned computer metrics history"
        );
    }
    Ok((fleet, deferred))
}

/// Spawn the leader-gated retention loop.
pub fn spawn_retention_loop(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = evaluate_retention(&pg).await {
                        warn!(error = %e, "task_retention tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("task_retention loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RETENTION_DAYS, mode_is_off, retention_days};

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("keep")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }

    #[test]
    fn retention_window_floors_at_one_day() {
        assert_eq!(retention_days(Some("14")), 14);
        assert_eq!(retention_days(Some(" 30 ")), 30);
        // Missing → default; junk/zero/negative → floored at 1 (never wipe live).
        assert_eq!(retention_days(None), DEFAULT_RETENTION_DAYS);
        assert_eq!(retention_days(Some("0")), 1);
        assert_eq!(retention_days(Some("-5")), 1);
        assert_eq!(retention_days(Some("banana")), DEFAULT_RETENTION_DAYS);
    }
}
