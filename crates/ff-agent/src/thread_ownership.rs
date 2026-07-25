//! Concurrency control for `workstream_threads` — the per-client-attach layer
//! under a project's single durable workstream (see [`crate::workstreams`]).
//!
//! Enforces the mechanisms the operator specified for concurrent access to a
//! thread:
//!   - **owner** — `owner_session` / `owner_acquired_at`: the session that
//!     currently holds write authority.
//!   - **presence** — `last_seen_at`: a heartbeat so a dead client's lock can
//!     be reclaimed instead of wedging the thread forever.
//!   - **causal seq** — `causal_seq`: a monotonic per-thread counter stamped
//!     on every accepted write, so a reclaimed owner's late/out-of-order
//!     writes can be detected and dropped by readers.
//!   - **single-writer / thread exclusivity** — [`try_acquire_thread_owner`]
//!     is a compare-and-swap: granted only when the thread is unowned,
//!     already owned by the caller, or the current owner's presence has gone
//!     stale past the caller's lease TTL. Exactly one session can hold the
//!     lock at a time.
//!   - **redaction** — [`redact_thread`]: tombstones a thread's content
//!     without deleting the row.

use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// Outcome of a single-writer acquire attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// Caller now (or still) holds exclusive ownership of the thread.
    Granted,
    /// Another session holds the thread and its presence is still fresh.
    HeldByOther { owner_session: String },
    /// No thread with that id.
    NotFound,
}

/// Attempt to acquire exclusive (single-writer) ownership of `thread_id` for
/// `session_id`. Granted when the thread is unowned, already owned by this
/// session (idempotent re-acquire), or the current owner's presence
/// heartbeat is older than `lease_ttl` (stale-owner reclaim).
pub async fn try_acquire_thread_owner(
    pg: &PgPool,
    thread_id: Uuid,
    session_id: &str,
    lease_ttl: Duration,
) -> Result<AcquireOutcome> {
    let lease_ttl_secs = lease_ttl.as_secs_f64();
    let acquired: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE workstream_threads \
            SET owner_session = $2, owner_acquired_at = now(), last_seen_at = now() \
          WHERE id = $1 \
            AND (owner_session IS NULL \
                 OR owner_session = $2 \
                 OR last_seen_at < now() - make_interval(secs => $3)) \
         RETURNING id",
    )
    .bind(thread_id)
    .bind(session_id)
    .bind(lease_ttl_secs)
    .fetch_optional(pg)
    .await?;

    if acquired.is_some() {
        return Ok(AcquireOutcome::Granted);
    }

    let current: Option<(Option<String>,)> =
        sqlx::query_as("SELECT owner_session FROM workstream_threads WHERE id = $1")
            .bind(thread_id)
            .fetch_optional(pg)
            .await?;

    Ok(match current {
        None => AcquireOutcome::NotFound,
        Some((owner,)) => AcquireOutcome::HeldByOther {
            owner_session: owner.unwrap_or_default(),
        },
    })
}

/// Renew presence for the current owner. Returns `false` if `session_id` is
/// not (or no longer) the owner — the caller must re-acquire before writing.
pub async fn heartbeat_presence(pg: &PgPool, thread_id: Uuid, session_id: &str) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE workstream_threads SET last_seen_at = now() \
          WHERE id = $1 AND owner_session = $2",
    )
    .bind(thread_id)
    .bind(session_id)
    .execute(pg)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Release ownership held by `session_id`. No-op if another session already
/// holds (or reclaimed) the thread.
pub async fn release_thread_owner(pg: &PgPool, thread_id: Uuid, session_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE workstream_threads \
            SET owner_session = NULL, owner_acquired_at = NULL \
          WHERE id = $1 AND owner_session = $2",
    )
    .bind(thread_id)
    .bind(session_id)
    .execute(pg)
    .await?;
    Ok(())
}

/// Stamp the next causal sequence number for a write to `thread_id`. Callers
/// attach the returned value to their write so readers can detect and drop
/// out-of-order writes from a reclaimed (stale) owner.
pub async fn next_causal_seq(pg: &PgPool, thread_id: Uuid) -> Result<i64> {
    let (seq,): (i64,) = sqlx::query_as(
        "UPDATE workstream_threads SET causal_seq = causal_seq + 1 \
          WHERE id = $1 RETURNING causal_seq",
    )
    .bind(thread_id)
    .fetch_one(pg)
    .await?;
    Ok(seq)
}

/// Redact a thread's content: tombstones `focus`/`label`, records why, and
/// force-releases any current owner — a redacted thread is no longer
/// writable.
pub async fn redact_thread(pg: &PgPool, thread_id: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE workstream_threads \
            SET focus = NULL, label = NULL, \
                redacted_at = now(), redacted_reason = $2, \
                owner_session = NULL, owner_acquired_at = NULL, \
                status = 'redacted' \
          WHERE id = $1",
    )
    .bind(thread_id)
    .bind(reason)
    .execute(pg)
    .await?;
    Ok(())
}
