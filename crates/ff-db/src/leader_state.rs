//! CRUD helpers for the `fleet_leader_state` singleton table (Schema V14).
//!
//! The table has a single row keyed by `singleton_key = 'current'`. These
//! helpers are used by the leader-election tick running on every daemon:
//!
//! - [`pg_get_current_leader`] — read the current leader row.
//! - [`pg_claim_leader_initial`] — try to INSERT when the table is empty.
//! - [`pg_claim_leader_takeover`] — conditional UPDATE to steal leadership
//!   from a stale/dead leader.
//! - [`pg_refresh_leader_heartbeat`] — current leader re-asserts ownership.
//! - [`pg_yield_leader`] — current leader relinquishes.
//!
//! All functions take `&PgPool` and return `sqlx::Error` on failure.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A snapshot of the `fleet_leader_state` row.
#[derive(Debug, Clone)]
pub struct LeaderState {
    pub computer_id: Uuid,
    pub member_name: String,
    pub epoch: i64,
    pub elected_at: DateTime<Utc>,
    pub reason: Option<String>,
    pub heartbeat_at: DateTime<Utc>,
}

/// Read the current leader, if any.
pub async fn pg_get_current_leader(pool: &PgPool) -> Result<Option<LeaderState>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT computer_id, member_name, epoch, elected_at, reason, heartbeat_at
         FROM fleet_leader_state
         WHERE singleton_key = 'current'",
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| LeaderState {
        computer_id: r.get("computer_id"),
        member_name: r.get("member_name"),
        epoch: r.get("epoch"),
        elected_at: r.get("elected_at"),
        reason: r.get("reason"),
        heartbeat_at: r.get("heartbeat_at"),
    }))
}

/// INSERT the singleton row (first claim). Returns `true` if this call
/// successfully inserted the row; `false` if it already existed.
pub async fn pg_claim_leader_initial(
    pool: &PgPool,
    computer_id: Uuid,
    member_name: &str,
    epoch: i64,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO fleet_leader_state (singleton_key, computer_id, member_name, epoch, reason)
         VALUES ('current', $1, $2, $3, $4)
         ON CONFLICT (singleton_key) DO NOTHING",
    )
    .bind(computer_id)
    .bind(member_name)
    .bind(epoch)
    .bind(reason)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Conditional UPDATE: steal leadership iff the existing row is stale
/// (older than `stale_threshold_secs`) AND its epoch is strictly less than
/// our proposed new epoch AND the old leader's name matches what we
/// observed when we decided to challenge. Returns `true` if the row was
/// replaced.
pub async fn pg_claim_leader_takeover(
    pool: &PgPool,
    my_computer_id: Uuid,
    my_name: &str,
    new_epoch: i64,
    old_leader_name: &str,
    stale_threshold_secs: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE fleet_leader_state
         SET computer_id  = $1,
             member_name  = $2,
             epoch        = $3,
             elected_at   = NOW(),
             reason       = 'takeover',
             heartbeat_at = NOW()
         WHERE singleton_key = 'current'
           AND member_name = $4
           AND epoch < $3
           AND heartbeat_at < NOW() - make_interval(secs => $5)",
    )
    .bind(my_computer_id)
    .bind(my_name)
    .bind(new_epoch)
    .bind(old_leader_name)
    .bind(stale_threshold_secs.max(0) as f64)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Refresh `heartbeat_at` if we are still the leader. Returns `true` iff
/// a row was updated — useful for detecting that we have been displaced.
pub async fn pg_refresh_leader_heartbeat(
    pool: &PgPool,
    my_name: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE fleet_leader_state
         SET heartbeat_at = NOW()
         WHERE singleton_key = 'current'
           AND member_name  = $1",
    )
    .bind(my_name)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Relinquish leadership by deleting the singleton row. Returns `true`
/// iff a row was deleted (i.e. we really were the leader).
pub async fn pg_yield_leader(pool: &PgPool, my_name: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM fleet_leader_state
         WHERE singleton_key = 'current'
           AND member_name   = $1",
    )
    .bind(my_name)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}
