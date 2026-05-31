//! Orchestrator P2 — Per-session demand sensing tick.
//!
//! Leader-gated loop (mirrors [`crate::scheduler_tick::spawn_scheduler_tick`])
//! that, every `interval_secs` on the leader, recomputes the live fleet-wide
//! demand vector (`ff_db::pg_current_demand_vector`), snapshots it into one
//! `fleet_demand_snapshot` row, and prunes snapshots older than 6h.
//!
//! The snapshot is the contract P3 (the adaptive serving-mix autoscaler)
//! consumes: it reads one cheap indexed row instead of re-aggregating. P2 only
//! PRODUCES the demand signal — it never loads/unloads a model. If P3 isn't
//! deployed, snapshots accumulate harmlessly and are pruned at 6h, so this is
//! fully backward-safe.
//!
//! Cost: two indexed queries + one insert per `interval_secs`, on the leader
//! only.

use anyhow::Result;
use sqlx::PgPool;
use tracing::{debug, info, warn};

/// How long a demand snapshot is retained before pruning.
const SNAPSHOT_RETENTION: &str = "6 hours";
/// Aggregation window for the demand vector (seconds).
const DEMAND_WINDOW_SECS: i64 = 300;

/// Recompute the demand vector, persist one snapshot row, and prune old rows.
///
/// Returns the number of code+general slots wanted (for logging only).
pub async fn snapshot_demand(pg: &PgPool) -> Result<()> {
    let vector = ff_db::pg_current_demand_vector(pg, DEMAND_WINDOW_SECS).await?;

    sqlx::query(
        r#"
        INSERT INTO fleet_demand_snapshot
            (window_secs, active_sessions, code_slots_wanted,
             general_slots_wanted, per_session)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(vector.window_secs as i32)
    .bind(vector.active_sessions)
    .bind(vector.code_slots_wanted)
    .bind(vector.general_slots_wanted)
    .bind(&vector.per_session)
    .execute(pg)
    .await?;

    // Cheap retention: drop snapshots older than the retention window.
    let pruned = sqlx::query(&format!(
        "DELETE FROM fleet_demand_snapshot \
         WHERE captured_at < NOW() - INTERVAL '{SNAPSHOT_RETENTION}'"
    ))
    .execute(pg)
    .await?
    .rows_affected();

    if vector.active_sessions > 0 {
        info!(
            active_sessions = vector.active_sessions,
            code_slots_wanted = vector.code_slots_wanted,
            general_slots_wanted = vector.general_slots_wanted,
            pruned,
            "demand snapshot recorded"
        );
    } else {
        debug!(pruned, "demand snapshot: no active sessions");
    }

    Ok(())
}

/// Spawn the leader-gated demand-sensing loop. The gate is read from Postgres
/// `fleet_leader_state` exactly like [`crate::scheduler_tick::spawn_scheduler_tick`].
pub fn spawn_demand_tick(
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
                            WHERE leader_name = $1
                              AND last_heartbeat > NOW() - INTERVAL '60 seconds'
                        )
                        "#
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    if let Err(e) = snapshot_demand(&pg).await {
                        warn!(error = %e, "demand sensor tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("demand sensor tick loop stopped");
    })
}
