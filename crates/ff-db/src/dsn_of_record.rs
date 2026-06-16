//! DSN of record — the durable home for the current Postgres primary DSN.
//!
//! HA leader-handoff Phase 3 (design `plans/ha-leader-handoff.md`, open
//! question Q2) needs to answer: *after a primary MOVE, how does a worker —
//! which holds a STATIC per-host DSN — learn the new primary?*
//!
//! The mechanism, in order of authority:
//!   1. The `dsn_of_record` singleton table (schema V136) — the auditable home
//!      of record (who repointed, when, the prior value for rollback).
//!   2. The `db_dsn_of_record` fleet_secret — a mirror of (1) so the value lives
//!      in the same place as every other fleet-wide knob and is trivially
//!      readable by [`pg_get_secret`].
//!   3. A node-local cache file (`~/.forgefleet/db_dsn_of_record`) — the LAST
//!      resort that lets a worker recover when its STATIC DSN is dead *and*
//!      Postgres is unreachable (so neither (1) nor (2) can be read). The cache
//!      is refreshed every time a worker successfully reads (2) from a live DB.
//!
//! This module is pure plumbing: read/write the row + secret + cache. It does
//! NOT promote anything and never runs automatically — it is written only by an
//! explicit operator handoff (`ff fleet db handoff --execute`).

use sqlx::PgPool;

// Cache primitives live in ff-core::db so the connect path (which cannot depend
// on ff-db) and this writer share ONE definition of the cache file + path.
pub use ff_core::db::{DSN_OF_RECORD_CACHE_FILE, dsn_cache_path, read_dsn_cache, write_dsn_cache};

use crate::error::Result;
use crate::queries::{pg_get_secret, pg_set_secret};

/// `fleet_secrets` key mirroring the current primary DSN. Workers read this on
/// connect-failure; absent ⇒ fall back to the static DSN (fail-safe).
pub const DSN_OF_RECORD_SECRET_KEY: &str = "db_dsn_of_record";

/// The current DSN of record, read in authority order: table row, then the
/// `db_dsn_of_record` fleet_secret. Returns `None` when neither is set (the
/// inert default — caller must fall back to its static DSN).
///
/// On a successful non-empty read the value is also written to the node-local
/// cache ([`write_dsn_cache`]) so a future connect-failure (when Postgres is
/// unreachable) can still recover.
pub async fn read_dsn_of_record(pool: &PgPool) -> Result<Option<String>> {
    // Prefer the table row (the home of record); fall back to the secret mirror.
    let from_row: Option<String> =
        sqlx::query_scalar("SELECT dsn FROM dsn_of_record WHERE singleton_key = 'current'")
            .fetch_optional(pool)
            .await?;
    let value = match from_row {
        Some(dsn) => Some(dsn),
        None => pg_get_secret(pool, DSN_OF_RECORD_SECRET_KEY).await?,
    };
    if let Some(ref dsn) = value
        && !dsn.trim().is_empty()
    {
        write_dsn_cache(dsn);
    }
    Ok(value)
}

/// Repoint the DSN of record to `new_dsn`. Writes the singleton table row
/// (recording `previous_dsn` for rollback) AND mirrors into the
/// `db_dsn_of_record` fleet_secret in one logical step.
///
/// This is the ONLY writer, and it is invoked exclusively from the operator
/// handoff's `--execute` path — never from a tick.
pub async fn repoint_dsn_of_record(
    pool: &PgPool,
    new_dsn: &str,
    primary_member: Option<&str>,
    updated_by: Option<&str>,
) -> Result<()> {
    // 1) Upsert the singleton row, carrying the old dsn into previous_dsn.
    sqlx::query(
        "INSERT INTO dsn_of_record (singleton_key, dsn, primary_member, previous_dsn, updated_at, updated_by)
         VALUES ('current', $1, $2, NULL, NOW(), $3)
         ON CONFLICT (singleton_key) DO UPDATE SET
             previous_dsn  = dsn_of_record.dsn,
             dsn           = EXCLUDED.dsn,
             primary_member = EXCLUDED.primary_member,
             updated_at    = NOW(),
             updated_by    = EXCLUDED.updated_by",
    )
    .bind(new_dsn)
    .bind(primary_member)
    .bind(updated_by)
    .execute(pool)
    .await?;

    // 2) Mirror into the fleet_secret so workers reading the secret see it too.
    pg_set_secret(
        pool,
        DSN_OF_RECORD_SECRET_KEY,
        new_dsn,
        Some("current Postgres primary DSN (HA Phase 3 handoff)"),
        updated_by,
    )
    .await?;

    Ok(())
}
