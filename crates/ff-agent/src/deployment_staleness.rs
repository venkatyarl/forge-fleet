//! Leader-gated deployment-staleness tick — the write-side companion to #369.
//!
//! #369 made `ff model deployments` *render* `stale` for offline-owned rows by
//! cross-referencing live pulse beats. But the stored
//! `fleet_model_deployments.health_status` still says `healthy`, because a
//! deployment row's health is only ever written by its OWNING node's local
//! `deployment_reconciler` — and a dead node never runs it. So the DB itself
//! keeps lying to every OTHER consumer that reads the column directly (the
//! agent router, MCP, `ff model coverage`): a long-dead host (ace, ~16h down)
//! advertises a `healthy` endpoint forever.
//!
//! This leader-only tick flips `health_status='stale'` for active deployments
//! whose owning computer hasn't beaten in [`OFFLINE_THRESHOLD`], using the same
//! materialized liveness signal (`computers.last_seen_at`) the rest of the
//! fleet uses. It is self-correcting and never fights the per-node writer: when
//! the node returns, its `last_seen_at` is fresh (so this tick skips it) and
//! its reconciler overwrites the real status on the next beat.
//!
//! Safety: it NEVER masks on a global signal loss. If the materializer is down
//! (no computer is fresh) the sweep is skipped entirely, mirroring #369's
//! conservative "if pulse unavailable, treat all online" stance — we must not
//! stale the whole fleet on our own blindness.

use sqlx::PgPool;
use tracing::{info, warn};

/// A computer silent at least this long has its deployments treated as stale.
/// Beats fire every ~5-15s, so 5 minutes is unambiguously down (not a transient
/// miss) — deliberately more conservative than the read-side view, which flips
/// on a single absent beat because it re-derives per command and mutates
/// nothing.
const OFFLINE_THRESHOLD: &str = "5 minutes";

/// A computer seen at least this recently proves the materializer is alive.
/// Used only by the global-signal-loss guard.
const FRESH_THRESHOLD: &str = "2 minutes";

/// The safety invariant, isolated so it is unit-testable: run the staling sweep
/// only when at least one computer is currently fresh. Zero fresh computers
/// means the materializer is presumably dead and `last_seen_at` is globally
/// stale — staling on that signal would wrongly mark every deployment in the
/// fleet, so we skip.
fn should_run_sweep(fresh_computer_count: i64) -> bool {
    fresh_computer_count > 0
}

/// Mark active deployments of offline-owned computers `stale` in the DB.
/// Returns the number of rows flipped (0 when nothing needed it, or when the
/// global-signal-loss guard tripped).
pub async fn mark_offline_deployments_stale(pg: &PgPool) -> Result<u64, sqlx::Error> {
    // Guard: don't mistake a dead materializer for "every node offline".
    let fresh: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM computers \
          WHERE last_seen_at > NOW() - INTERVAL '{FRESH_THRESHOLD}'"
    ))
    .fetch_one(pg)
    .await?;
    if !should_run_sweep(fresh) {
        warn!(
            "deployment-staleness: no computer beat within {FRESH_THRESHOLD} — \
             materializer may be down; skipping (never mask on global signal loss)"
        );
        return Ok(0);
    }

    // Flip only active rows whose owning computer is demonstrably offline and
    // that aren't already `stale`. The node's own reconciler reclaims the row
    // (back to a real status) on its next beat once it returns.
    let flipped = sqlx::query(&format!(
        "UPDATE fleet_model_deployments d \
            SET health_status = 'stale' \
           FROM computers c \
          WHERE c.name = d.worker_name \
            AND d.desired_state = 'active' \
            AND d.health_status <> 'stale' \
            AND c.last_seen_at < NOW() - INTERVAL '{OFFLINE_THRESHOLD}'"
    ))
    .execute(pg)
    .await?
    .rows_affected();
    Ok(flipped)
}

/// Spawn the leader-gated deployment-staleness loop. The leader gate is read
/// from Postgres `fleet_leader_state` exactly like
/// [`crate::demand_sensor::spawn_demand_tick`].
pub fn spawn_deployment_staleness_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE member_name = $1
                              AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                        )
                        "#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    match mark_offline_deployments_stale(&pg).await {
                        Ok(n) if n > 0 => info!(
                            flipped = n,
                            "deployment-staleness: marked offline-owned deployment(s) stale"
                        ),
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "deployment-staleness tick failed"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("deployment-staleness tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::should_run_sweep;

    #[test]
    fn skips_sweep_when_no_computer_is_fresh() {
        // Materializer presumed down → never stale the whole fleet.
        assert!(!should_run_sweep(0));
    }

    #[test]
    fn runs_sweep_when_any_computer_is_fresh() {
        assert!(should_run_sweep(1));
        assert!(should_run_sweep(14));
    }
}
