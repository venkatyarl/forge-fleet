//! Leader-gated metrics-partition maintenance.
//!
//! The tiered fleet metrics tables (V177: `fleet_metrics_raw` /
//! `fleet_metrics_1min` / `fleet_metrics_hourly`) are range-partitioned
//! parents whose dated children are managed at runtime, not by migrations.
//! Each tick pre-creates upcoming children (so writers always have a
//! partition to land in) and drops expired ones (raw > 7d, 1min rollups >
//! 30d; hourly kept forever) — retention as cheap partition DROPs.
//!
//! Leader-gated, runs by DEFAULT; opt out with
//! `fleet_secrets.metrics_partition_mode=off`.

use anyhow::Result;
use sqlx::PgPool;
use tracing::{info, warn};

const MODE_KEY: &str = "metrics_partition_mode";

/// `off`/`false`/`0`/`disabled`/`no` (case-insensitive) disable the tick; any
/// other value — including a missing secret — leaves it ON.
fn mode_is_off(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("off" | "false" | "0" | "disabled" | "no")
    )
}

/// One maintenance pass. Leader-only; returns the dropped partition names.
pub async fn evaluate_partition_maintenance(pg: &PgPool) -> Result<Vec<String>> {
    if !crate::leader_cache::is_current_leader() {
        return Ok(Vec::new());
    }
    let mode = ff_db::pg_get_secret(pg, MODE_KEY).await.ok().flatten();
    if mode_is_off(mode.as_deref()) {
        return Ok(Vec::new());
    }
    let now = chrono::Utc::now();
    ff_db::pg_ensure_metrics_partitions(pg, now).await?;
    let dropped = ff_db::pg_drop_expired_metrics_partitions(pg, now).await?;
    if !dropped.is_empty() {
        info!(
            dropped = dropped.len(),
            partitions = ?dropped,
            "metrics_partition_maintenance: dropped expired metric partitions"
        );
    }
    Ok(dropped)
}

/// Spawn the leader-gated partition-maintenance loop.
pub fn spawn_metrics_partition_loop(
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
                    if let Err(e) = evaluate_partition_maintenance(&pg).await {
                        warn!(error = %e, "metrics_partition_maintenance tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("metrics_partition_maintenance loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::mode_is_off;

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("keep")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }
}
