//! Auto-recycler for work_items that died on TRANSIENT infrastructure errors
//! (the "self-heal recycler").
//!
//! When a work_item reaches terminal `failed` on an error the coding agent
//! could never have fixed (backend spawn, pool exhaustion, network, lease
//! lifecycle — the same infra class [`crate::work_item_dispatch`] filters out
//! of retry prompts), the fleet can recycle it back into the backlog instead
//! of leaving it dead. Recycling:
//!   - sets `status = 'ready'` so the scheduler re-dispatches it;
//!   - resets `attempts` to 0 for general transients (fresh escalation
//!     ladder), but to 1 for STALL-class failures (heartbeat starvation /
//!     stale-lease reap / timeouts) so the retry starts one rung up the
//!     ladder instead of replaying the exact run that stalled;
//!   - increments `metadata.recycle_count`, the recycler's own budget kept
//!     separate from the dispatch `attempts` counter it resets. An item is
//!     recycled at most [`MAX_RECYCLE_COUNT`] times; past the cap the item
//!     stays `failed` and [`recycle_work_item`] returns `false`.
//!
//! Duplicate recycle requests are idempotent: the guarded UPDATE only fires
//! while the item is still `failed`, so a second request finds it already
//! `ready` with a non-zero `recycle_count` and reports success WITHOUT a
//! second increment.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// Max times a work_item may be auto-recycled before it stays `failed` for a
/// human to look at.
pub const MAX_RECYCLE_COUNT: i32 = 3;

/// How a transient failure should be recycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Generic transient infra error (network, pool, provider 5xx, spawn):
    /// nothing suggests the prior lane was at fault, so restart the
    /// escalation ladder from scratch (`attempts = 0`).
    Transient,
    /// STALL-class failure: the run starved its heartbeat, hit a timeout, or
    /// was reaped on a stale lease. Restart with `attempts = 1` so the retry
    /// counts the stalled run against the local codegen lane's try budget and
    /// escalates toward the cloud backstop sooner.
    Stall,
}

/// Error signatures that mark a STALL-class failure, matched
/// case-insensitively against the item's `last_error`. Drawn from the live
/// stall/reap errors dispatch records ("stale-heartbeat", "heartbeat
/// takeover", lane timeouts, "3 stalled attempts").
const STALL_SIGNATURES: &[&str] = &[
    "stall",
    "stale-heartbeat",
    "heartbeat takeover",
    "timed out",
    "timeout",
    "reaped",
];

impl FailureClass {
    /// Classify a stored `last_error` into the recycle class to use.
    pub fn classify(last_error: &str) -> Self {
        let lower = last_error.to_ascii_lowercase();
        if STALL_SIGNATURES.iter().any(|sig| lower.contains(sig)) {
            FailureClass::Stall
        } else {
            FailureClass::Transient
        }
    }

    /// The `attempts` value a recycled item restarts with.
    pub fn recycled_attempts(self) -> i32 {
        match self {
            FailureClass::Transient => 0,
            FailureClass::Stall => 1,
        }
    }
}

/// Recycle a `failed` work_item back to `ready`, resetting `attempts` per
/// [`FailureClass`] and incrementing `metadata.recycle_count`.
///
/// Returns `true` when the item was recycled (or a duplicate request found it
/// already recycled — idempotent, no second increment), `false` when the
/// recycle budget is exhausted (`recycle_count >= MAX_RECYCLE_COUNT`), the
/// item is unknown, or it isn't in a recyclable state.
pub async fn recycle_work_item(
    pg: &PgPool,
    work_item_id: Uuid,
    class: FailureClass,
) -> Result<bool> {
    // Single guarded UPDATE so concurrent/duplicate recycle requests can't
    // double-increment: only a still-`failed` item under the budget matches.
    let recycled = sqlx::query(
        "UPDATE work_items
            SET status = 'ready',
                attempts = $2,
                metadata = jsonb_set(
                    COALESCE(metadata, '{}'::jsonb),
                    '{recycle_count}',
                    to_jsonb(COALESCE((metadata->>'recycle_count')::int, 0) + 1),
                    true)
          WHERE id = $1
            AND status = 'failed'
            AND COALESCE((metadata->>'recycle_count')::int, 0) < $3",
    )
    .bind(work_item_id)
    .bind(class.recycled_attempts())
    .bind(MAX_RECYCLE_COUNT)
    .execute(pg)
    .await?;
    if recycled.rows_affected() > 0 {
        return Ok(true);
    }

    // No row updated: either a duplicate request (item already recycled back
    // to `ready`) — report success without touching it — or the item is
    // unknown / budget-exhausted / not recyclable → false.
    let already_recycled: Option<bool> = sqlx::query_scalar(
        "SELECT status = 'ready'
                AND COALESCE((metadata->>'recycle_count')::int, 0) > 0
           FROM work_items
          WHERE id = $1",
    )
    .bind(work_item_id)
    .fetch_optional(pg)
    .await?;
    Ok(already_recycled.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_stall_signatures() {
        for err in [
            "run reaped on stale-heartbeat",
            "heartbeat takeover by another node",
            "command timed out after 190s",
            "lane-1 TIMEOUT waiting for codegen",
            "3 stalled attempts",
        ] {
            assert_eq!(FailureClass::classify(err), FailureClass::Stall, "{err}");
        }
    }

    #[test]
    fn classify_general_transients() {
        for err in [
            "connection refused",
            "no dispatchable backend on this node",
            "service unavailable",
            "pool exhausted acquiring connection",
        ] {
            assert_eq!(
                FailureClass::classify(err),
                FailureClass::Transient,
                "{err}"
            );
        }
    }

    #[test]
    fn recycled_attempts_per_class() {
        // General transients restart the ladder from scratch; STALL-class
        // restarts at 1 so the stalled run counts against the local lane's
        // try budget.
        assert_eq!(FailureClass::Transient.recycled_attempts(), 0);
        assert_eq!(FailureClass::Stall.recycled_attempts(), 1);
    }

    /// DB round-trip: recycle, idempotent duplicate, and the recycle_count
    /// cap. Early-returns when no Postgres is configured (CI has none); uses
    /// a session-local TEMPORARY work_items table on a single-connection pool
    /// so the real table is never touched.
    #[tokio::test]
    async fn recycle_updates_idempotently_until_cap() {
        let database_url = match std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping recycler DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
                return;
            }
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect to Postgres");
        sqlx::raw_sql(
            "CREATE TEMPORARY TABLE work_items (
                 id       UUID PRIMARY KEY,
                 status   TEXT NOT NULL,
                 attempts INT  NOT NULL DEFAULT 0,
                 metadata JSONB NOT NULL DEFAULT '{}'::jsonb
             );",
        )
        .execute(&pool)
        .await
        .expect("create temporary work_items table");

        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO work_items (id, status, attempts) VALUES ($1, 'failed', 2)")
            .bind(id)
            .execute(&pool)
            .await
            .expect("insert failed work_item");

        let state = |pool: &PgPool| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (String, i32, i32)>(
                    "SELECT status, attempts,
                            COALESCE((metadata->>'recycle_count')::int, 0)
                       FROM work_items WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("read work_item state")
            }
        };

        // 1st recycle (general transient): ready, attempts reset, count 1.
        assert!(
            recycle_work_item(&pool, id, FailureClass::Transient)
                .await
                .expect("recycle #1")
        );
        assert_eq!(state(&pool).await, ("ready".into(), 0, 1));

        // Duplicate request while already recycled: idempotent success, no
        // second increment.
        assert!(
            recycle_work_item(&pool, id, FailureClass::Transient)
                .await
                .expect("duplicate recycle")
        );
        assert_eq!(state(&pool).await, ("ready".into(), 0, 1));

        // STALL-class recycle restarts at attempts = 1.
        sqlx::query("UPDATE work_items SET status = 'failed', attempts = 3 WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("re-fail work_item");
        assert!(
            recycle_work_item(&pool, id, FailureClass::Stall)
                .await
                .expect("recycle #2")
        );
        assert_eq!(state(&pool).await, ("ready".into(), 1, 2));

        // 3rd recycle exhausts the budget…
        sqlx::query("UPDATE work_items SET status = 'failed' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("re-fail work_item");
        assert!(
            recycle_work_item(&pool, id, FailureClass::Transient)
                .await
                .expect("recycle #3")
        );
        assert_eq!(state(&pool).await, ("ready".into(), 0, 3));

        // …so a 4th is rejected: false, still failed, count unchanged.
        sqlx::query("UPDATE work_items SET status = 'failed' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("re-fail work_item");
        assert!(
            !recycle_work_item(&pool, id, FailureClass::Transient)
                .await
                .expect("recycle past cap")
        );
        assert_eq!(state(&pool).await, ("failed".into(), 0, 3));

        // Unknown item → false.
        assert!(
            !recycle_work_item(&pool, Uuid::new_v4(), FailureClass::Transient)
                .await
                .expect("recycle unknown item")
        );
    }
}
