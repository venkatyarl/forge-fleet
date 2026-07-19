//! Postgres connection pool setup with DSN-of-record failover.
//!
//! Mirrors the connection-pool primitives in `ff_core::db` so ff-db consumers
//! can build a pool with the same HA Phase 3 failover semantics used by the
//! daemon. The node-local DSN cache is shared with ff-core; this module is a
//! thin adapter around it.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing::{info, warn};

use ff_core::config::DatabaseConfig;
use ff_core::db::read_dsn_cache;

use crate::error::{DbError, Result};

/// Create a Postgres connection pool from fleet config, honouring DSN failover.
///
/// 1. Always try the static [`DatabaseConfig::url`] first.
/// 2. If that fails AND `db_config.dsn_failover` is `true`, read the last-known
///    DSN of record from the node-local cache ([`read_dsn_cache`]) and retry
///    against it.
/// 3. If the flag is off, or no cache entry exists, or the cached DSN matches
///    the static one, the ORIGINAL static-DSN error is returned unchanged.
///
/// Fail-safe: with the default `dsn_failover = false` this is exactly equivalent
/// to a single `connect_url(static)`.
pub async fn create_pool_with_dsn_failover(db_config: &DatabaseConfig) -> Result<PgPool> {
    let static_url = db_config.url.trim();
    let static_err = match connect_url(static_url, db_config.max_connections).await {
        Ok(pool) => return Ok(pool),
        Err(e) => e,
    };

    if !db_config.dsn_failover {
        return Err(DbError::Pool(static_err.to_string()));
    }

    // Static DSN dead + failover opted-in: consult the DSN of record.
    let Some(record) = read_dsn_cache() else {
        warn!(
            "dsn-failover: static DSN unreachable and no DSN-of-record cache present — \
             returning original error"
        );
        return Err(DbError::Pool(static_err.to_string()));
    };
    if record.trim() == static_url {
        // Nothing new to try.
        return Err(DbError::Pool(static_err.to_string()));
    }

    warn!("dsn-failover: static DSN unreachable — retrying against cached DSN of record");
    match connect_url(record.trim(), db_config.max_connections).await {
        Ok(pool) => {
            info!("dsn-failover: connected via DSN of record");
            Ok(pool)
        }
        Err(e) => {
            warn!(error = %e, "dsn-failover: DSN-of-record connect also failed");
            // Surface the ORIGINAL static error — it's the user's configured DSN.
            Err(DbError::Pool(static_err.to_string()))
        }
    }
}

/// Create a Postgres connection pool from fleet config (no failover).
pub async fn create_pool(db_config: &DatabaseConfig) -> Result<PgPool> {
    connect_url(db_config.url.trim(), db_config.max_connections)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))
}

/// Shared pool builder used by [`create_pool`] and [`create_pool_with_dsn_failover`]
/// so both paths produce identically-configured pools.
async fn connect_url(url: &str, max_connections: u32) -> sqlx::Result<PgPool> {
    let connect_options: PgConnectOptions = url.parse()?;
    PgPoolOptions::new()
        .max_connections(max_connections.max(2))
        .min_connections(0)
        .idle_timeout(Some(Duration::from_secs(60)))
        .max_lifetime(Some(Duration::from_secs(30 * 60)))
        .connect_with(connect_options)
        .await
}

/// Check if the database is reachable with a simple query.
pub async fn health_check(pool: &PgPool) -> Result<()> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map_err(DbError::Postgres)?;
    Ok(())
}

/// Gracefully close the pool, waiting for active queries to finish.
pub async fn shutdown_pool(pool: PgPool) {
    info!("shutting down postgres pool…");
    pool.close().await;
    info!("postgres pool closed");
}

#[cfg(test)]
mod tests {
    use ff_core::db::{DSN_OF_RECORD_CACHE_FILE, read_dsn_cache};

    #[test]
    fn dsn_cache_file_constant_matches_ff_core() {
        assert_eq!(DSN_OF_RECORD_CACHE_FILE, "db_dsn_of_record");
    }

    #[test]
    fn dsn_cache_roundtrip_uses_ff_core_impl() {
        let tmp = std::env::temp_dir().join(format!("ffdb-postgres-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let prev = std::env::var_os("HOME");
        // SAFETY: single-threaded unit test; no concurrent env access.
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        // No file yet → None.
        assert_eq!(read_dsn_cache(), None);

        // An all-whitespace cache reads as None (fail-safe to static DSN).
        ff_core::db::write_dsn_cache("   ");
        assert_eq!(read_dsn_cache(), None);

        // Write then read back (trimmed).
        ff_core::db::write_dsn_cache("  postgres://u:p@10.0.0.9:55432/forgefleet\n");
        assert_eq!(
            read_dsn_cache().as_deref(),
            Some("postgres://u:p@10.0.0.9:55432/forgefleet")
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
