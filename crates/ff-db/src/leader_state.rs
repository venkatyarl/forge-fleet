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
    pub redis_url: Option<String>,
    pub nats_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaderEndpoints {
    pub redis_url: Option<String>,
    pub nats_url: Option<String>,
}

/// Read the current leader, if any.
pub async fn pg_get_current_leader(pool: &PgPool) -> Result<Option<LeaderState>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT computer_id, member_name, epoch, elected_at, reason, heartbeat_at,
                redis_url, nats_url
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
        redis_url: r.get("redis_url"),
        nats_url: r.get("nats_url"),
    }))
}

/// Resolve ephemeral control-plane endpoints from the current leader lease.
/// Environment variables remain a bootstrap concern; connected clients use
/// this row so a leader move does not require config redistribution.
pub async fn pg_get_leader_endpoints(pool: &PgPool) -> Result<LeaderEndpoints, sqlx::Error> {
    let row = sqlx::query(
        "SELECT redis_url, nats_url FROM fleet_leader_state
         WHERE singleton_key = 'current'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map_or_else(LeaderEndpoints::default, |r| LeaderEndpoints {
            redis_url: r.get("redis_url"),
            nats_url: r.get("nats_url"),
        }),
    )
}

/// INSERT the singleton row (first claim). Returns `true` if this call
/// successfully inserted the row; `false` if it already existed.
pub async fn pg_claim_leader_initial(
    pool: &PgPool,
    computer_id: Uuid,
    member_name: &str,
    epoch: i64,
    reason: &str,
    redis_url: Option<&str>,
    nats_url: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO fleet_leader_state
             (singleton_key, computer_id, member_name, epoch, reason, redis_url, nats_url)
         VALUES ('current', $1, $2, $3, $4, $5, $6)
         ON CONFLICT (singleton_key) DO NOTHING",
    )
    .bind(computer_id)
    .bind(member_name)
    .bind(epoch)
    .bind(reason)
    .bind(redis_url)
    .bind(nats_url)
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
    redis_url: Option<&str>,
    nats_url: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE fleet_leader_state
         SET computer_id  = $1,
             member_name  = $2,
             epoch        = $3,
             elected_at   = NOW(),
             reason       = 'takeover',
             heartbeat_at = NOW(),
             redis_url     = $6,
             nats_url      = $7
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
    .bind(redis_url)
    .bind(nats_url)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Pulse-silent challenge takeover. Unlike [`pg_claim_leader_takeover`],
/// this does NOT require the leader's Postgres `heartbeat_at` to be stale
/// — it succeeds when the leader is silent on the Pulse v2 channel even
/// though the daemon is still refreshing Postgres. Closes the gap where
/// the Redis publisher hangs while the main daemon loop keeps writing
/// `heartbeat_at` to Postgres, leaving the leader invisible to peers and
/// un-replaceable via the heartbeat-age gate.
///
/// Caller must independently satisfy all of:
///   - Leader's `computers.name` is NOT in our local pulse `alive` map
///   - Silence has persisted for ≥ `MIN_PULSE_SILENT_SECS` (tracked in
///     [`LeaderTick::leader_pulse_silent_since`])
///   - Caller is the best-alive candidate per the usual priority rules
///
/// The SQL still enforces the epoch bump AND the old-leader-name match
/// (prevents two challengers from racing each other into split brain),
/// so concurrent callers will collapse to one winner via the Postgres
/// singleton lock.
pub async fn pg_claim_leader_pulse_silent(
    pool: &PgPool,
    my_computer_id: Uuid,
    my_name: &str,
    new_epoch: i64,
    old_leader_name: &str,
    redis_url: Option<&str>,
    nats_url: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE fleet_leader_state
         SET computer_id  = $1,
             member_name  = $2,
             epoch        = $3,
             elected_at   = NOW(),
             reason       = 'pulse_silent_challenge',
             heartbeat_at = NOW(),
             redis_url     = $5,
             nats_url      = $6
         WHERE singleton_key = 'current'
           AND member_name = $4
           AND epoch < $3",
    )
    .bind(my_computer_id)
    .bind(my_name)
    .bind(new_epoch)
    .bind(old_leader_name)
    .bind(redis_url)
    .bind(nats_url)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Refresh `heartbeat_at` if we are still the leader. Returns `true` iff
/// a row was updated — useful for detecting that we have been displaced.
pub async fn pg_refresh_leader_heartbeat(
    pool: &PgPool,
    my_name: &str,
    redis_url: Option<&str>,
    nats_url: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE fleet_leader_state
         SET heartbeat_at = NOW(), redis_url = $2, nats_url = $3
         WHERE singleton_key = 'current'
           AND member_name  = $1",
    )
    .bind(my_name)
    .bind(redis_url)
    .bind(nats_url)
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

/// HA Phase 2 — record a maintenance lease on the singleton leader row: while
/// the lease is live, election prefers `standby_member` outright. `until` is the
/// auto-fail-back deadline. Updates the existing row in place (the row may name a
/// different current leader — that's fine; the lease just biases the next pick).
pub async fn pg_set_maintenance_lease(
    pool: &PgPool,
    standby_member: &str,
    until: chrono::DateTime<chrono::Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE fleet_leader_state
            SET standby_member = $1, relinquishing_until = $2
          WHERE singleton_key = 'current'",
    )
    .bind(standby_member)
    .bind(until)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Clear any maintenance lease (immediate fail-back to normal election).
pub async fn pg_clear_maintenance_lease(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE fleet_leader_state
            SET standby_member = NULL, relinquishing_until = NULL
          WHERE singleton_key = 'current'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The currently-active maintenance lease, if any: returns `(standby_member,
/// relinquishing_until)` only when a standby is set AND the deadline is still in
/// the future. An expired lease reads as `None` (auto-fail-back) without needing
/// a write — the next step-down or a status read can lazily clear the columns.
pub async fn pg_get_active_maintenance_lease(
    pool: &PgPool,
) -> Result<Option<(String, chrono::DateTime<chrono::Utc>)>, sqlx::Error> {
    let row: Option<(Option<String>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT standby_member, relinquishing_until
           FROM fleet_leader_state
          WHERE singleton_key = 'current'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((Some(standby), Some(until))) if until > chrono::Utc::now() => Some((standby, until)),
        _ => None,
    })
}
