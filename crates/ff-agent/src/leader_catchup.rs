//! Leader catch-up via outbox replay.
//!
//! Mirrors [`crate::leader_tick`]'s election-driven scans (`revive_scan`,
//! `self_heal_scan`): it runs once, on demand, right after this node becomes
//! the fleet leader (cold claim or takeover). Postgres — not NATS — is the
//! durable record of task lifecycle events (`task_notification_outbox`,
//! schema V166), so any events written while the fleet was leaderless, or
//! while the previous leader died before it could relay them, sit with
//! `processed_at IS NULL`. [`LeaderCatchup::replay`] drains those rows to
//! NATS so subscribers eventually observe every event, not just the ones
//! published while a leader happened to be live.

use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::fleet_events_nats::FLEET_EVENTS_PREFIX;
use crate::nats_client;

/// Max unprocessed rows replayed in a single pass, so a very large backlog
/// (e.g. after extended leader downtime) can't stall the become-leader path
/// that calls this. Any remainder is simply picked up by the next replay.
const REPLAY_BATCH_LIMIT: i64 = 500;

/// Outcome of one [`LeaderCatchup::replay`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchupOutcome {
    /// Number of outbox rows claimed and published this pass.
    pub replayed: usize,
}

/// Replays missed `task_notification_outbox` events to NATS on leader
/// election. Construct fresh per replay — it holds only a pool handle.
pub struct LeaderCatchup {
    pg: PgPool,
}

impl LeaderCatchup {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Query the outbox for unprocessed rows and publish each to NATS.
    ///
    /// Each row is claimed with an atomic `UPDATE ... WHERE processed_at IS
    /// NULL` before it is published, so running this concurrently (e.g. two
    /// nodes racing a takeover) or repeatedly (e.g. a retried catch-up)
    /// never replays the same event twice.
    pub async fn replay(&self) -> Result<CatchupOutcome, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, task_id, event_type, payload
               FROM task_notification_outbox
              WHERE processed_at IS NULL
              ORDER BY created_at ASC
              LIMIT $1",
        )
        .bind(REPLAY_BATCH_LIMIT)
        .fetch_all(&self.pg)
        .await?;

        let mut replayed = 0usize;
        for row in rows {
            let id: i64 = row.get("id");

            let claim = sqlx::query(
                "UPDATE task_notification_outbox
                    SET processed_at = NOW()
                  WHERE id = $1 AND processed_at IS NULL",
            )
            .bind(id)
            .execute(&self.pg)
            .await?;
            if claim.rows_affected() == 0 {
                // Already claimed by a concurrent replay pass.
                continue;
            }

            let task_id: Uuid = row.get("task_id");
            let event_type: String = row.get("event_type");
            let payload: Value = row.get("payload");
            let subject = format!("{FLEET_EVENTS_PREFIX}.task.{event_type}");
            nats_client::publish_json(
                subject,
                &serde_json::json!({
                    "task_id": task_id,
                    "event_type": event_type,
                    "payload": payload,
                    "replayed": true,
                }),
            )
            .await;

            replayed += 1;
        }

        Ok(CatchupOutcome { replayed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()
    }

    #[tokio::test]
    async fn replay_is_idempotent_across_consecutive_passes() {
        let Some(url) = pg_url() else {
            eprintln!("skipping: no FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL set");
            return;
        };
        let pg = PgPool::connect(&url).await.expect("connect");

        let task_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO task_notification_outbox (task_id, event_type, payload)
             VALUES ($1, 'created', '{}'::jsonb)",
        )
        .bind(task_id)
        .execute(&pg)
        .await
        .expect("seed outbox row");

        let catchup = LeaderCatchup::new(pg.clone());
        let first = catchup.replay().await.expect("first replay");
        assert!(first.replayed >= 1, "seeded row should be replayed");

        let second = catchup.replay().await.expect("second replay");
        assert_eq!(
            second.replayed, 0,
            "already-processed rows must not replay again"
        );

        let _ = sqlx::query("DELETE FROM task_notification_outbox WHERE task_id = $1")
            .bind(task_id)
            .execute(&pg)
            .await;
    }
}
