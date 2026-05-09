//! Work Stealer (Phase 15d)
//!
//! Distributed work-item handoff watchdog. Every daemon runs this —
//! no leader gate. It finds `claimed`/`in_progress` fleet_work_items whose
//! assigned node has gone stale (no heartbeat / no progress for a
//! threshold) and yields them back to `pending` so any peer can claim
//! them.
//!
//! A separate "steal" path lets an idle node proactively find work
//! on an overloaded peer and transfer it atomically.

use std::time::Duration;

use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// How long a `claimed` or `in_progress` work item may sit without
/// progress before being yielded back to the pool.
const STUCK_ITEM_SECS: i64 = 180;
/// How often the handoff sweep runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Max times a work item can be handed off before it's marked failed.
const MAX_ITEM_HANDOFFS: i32 = 3;

/// Distributed watchdog: yield stale fleet_work_items back to pending.
///
/// Uses `FOR UPDATE SKIP LOCKED` so concurrent daemons race safely.
pub async fn handoff_stuck_work_items(pg: &PgPool) -> Result<usize, sqlx::Error> {
    // Demote stale claimed/in_progress items back to pending.
    let demoted = sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'pending',
               assigned_node_id = NULL,
               assigned_agent_id = NULL,
               assigned_session_id = NULL,
               yielded_at = NOW(),
               checkpoint_data = COALESCE(checkpoint_data, '{}') || jsonb_build_object('handoff_at', NOW()),
               retry_count = retry_count + 1
         WHERE id IN (
            SELECT id FROM fleet_work_items
             WHERE status IN ('claimed', 'in_progress')
               AND claimed_at < NOW() - make_interval(secs => $1::int)
               AND retry_count < $2
               FOR UPDATE SKIP LOCKED
         )
        RETURNING id
        "#,
    )
    .bind(STUCK_ITEM_SECS as i32)
    .bind(MAX_ITEM_HANDOFFS)
    .fetch_all(pg)
    .await?;

    // Permanently fail items that have exceeded max handoffs.
    let _ = sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'failed',
               completed_at = NOW(),
               error_message = 'exceeded MAX_ITEM_HANDOFFS retries'
         WHERE status IN ('claimed', 'in_progress')
           AND claimed_at < NOW() - make_interval(secs => $1::int)
           AND retry_count >= $2
        "#,
    )
    .bind(STUCK_ITEM_SECS as i32)
    .bind(MAX_ITEM_HANDOFFS)
    .execute(pg)
    .await?;

    Ok(demoted.len())
}

/// Proactive steal: an idle node finds a work item on an overloaded
/// peer and transfers it to itself.
///
/// Returns the stolen item ID, or None if no steal opportunity.
pub async fn try_steal_work_item(
    pg: &PgPool,
    thief_node_id: Uuid,
    thief_agent_id: Option<&str>,
) -> Result<Option<Uuid>, sqlx::Error> {
    // Only steal if this node has no pending work of its own.
    let my_pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_work_items WHERE assigned_node_id = $1 AND status IN ('pending', 'claimed', 'in_progress')"
    )
    .bind(thief_node_id)
    .fetch_one(pg)
    .await?;

    if my_pending > 0 {
        debug!(node = %thief_node_id, "skip steal: node has pending work");
        return Ok(None);
    }

    // Find an overloaded node (has multiple claimed/in_progress items).
    let victim = sqlx::query(
        r#"
        SELECT assigned_node_id, COUNT(*) as cnt
          FROM fleet_work_items
         WHERE status IN ('claimed', 'in_progress')
           AND assigned_node_id != $1
         GROUP BY assigned_node_id
        HAVING COUNT(*) > 1
         ORDER BY cnt DESC
         LIMIT 1
        "#,
    )
    .bind(thief_node_id)
    .fetch_optional(pg)
    .await?;

    let victim_id: Uuid = match victim {
        Some(row) => row.get("assigned_node_id"),
        None => return Ok(None),
    };

    // Atomically steal the oldest/heaviest item from the victim.
    let stolen: Option<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE fleet_work_items
           SET status = 'claimed',
               assigned_node_id = $1,
               assigned_agent_id = $2,
               claimed_at = NOW(),
               stolen_from = assigned_node_id,
               checkpoint_data = COALESCE(checkpoint_data, '{}') || jsonb_build_object('stolen_at', NOW())
         WHERE id = (
            SELECT id FROM fleet_work_items
             WHERE status IN ('claimed', 'in_progress')
               AND assigned_node_id = $3
             ORDER BY estimated_weight DESC, claimed_at ASC
               FOR UPDATE SKIP LOCKED
             LIMIT 1
         )
        RETURNING id
        "#,
    )
    .bind(thief_node_id)
    .bind(thief_agent_id)
    .bind(victim_id)
    .fetch_optional(pg)
    .await?;

    if let Some(id) = stolen {
        info!(item = %id, victim = %victim_id, thief = %thief_node_id, "work item stolen");
    }

    Ok(stolen)
}

/// Spawn the distributed work-item handoff watchdog.
pub fn spawn_work_item_watchdog(
    pg: PgPool,
    my_name: String,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match handoff_stuck_work_items(&pg).await {
                Ok(n) if n > 0 => {
                    info!(
                        handed_off = n,
                        node = %my_name,
                        "work-item watchdog re-queued stale items"
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "work-item watchdog query failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

/// Spawn a proactive steal loop. An idle node periodically attempts
/// to steal work from overloaded peers.
pub fn spawn_steal_loop(
    pg: PgPool,
    node_id: Uuid,
    agent_id: Option<String>,
    my_name: String,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(45);
        loop {
            match try_steal_work_item(&pg, node_id, agent_id.as_deref()).await {
                Ok(Some(id)) => {
                    info!(item = %id, node = %my_name, "proactive steal succeeded");
                }
                Ok(None) => {}
                Err(e) => debug!(error = %e, "proactive steal failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stuck_item_secs_constant() {
        assert_eq!(STUCK_ITEM_SECS, 180);
    }

    #[test]
    fn test_max_item_handoffs_constant() {
        assert_eq!(MAX_ITEM_HANDOFFS, 3);
    }

    #[test]
    fn test_sweep_interval() {
        assert_eq!(SWEEP_INTERVAL, Duration::from_secs(60));
    }
}
