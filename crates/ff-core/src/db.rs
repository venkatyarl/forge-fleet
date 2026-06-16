//! Postgres connection pool setup with sqlx.
//!
//! Provides a thin wrapper around `sqlx::PgPool` with config-driven setup,
//! health checks, and graceful shutdown.

use std::path::PathBuf;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing::{info, warn};

use crate::config::DatabaseConfig;
use crate::error::{ForgeFleetError, Result};

/// Basename of the node-local "DSN of record" cache under `~/.forgefleet/`.
///
/// HA leader-handoff Phase 3 (Q2): after an operator-driven Postgres primary
/// MOVE, a worker's STATIC DSN points at the old (now demoted/down) host. This
/// cache is the last-resort source of the current primary DSN when Postgres
/// itself is unreachable (so the `db_dsn_of_record` fleet_secret cannot be read
/// either). It is refreshed by ff-db whenever the secret is read from a LIVE DB.
pub const DSN_OF_RECORD_CACHE_FILE: &str = "db_dsn_of_record";

/// Path to the node-local DSN-of-record cache (`~/.forgefleet/db_dsn_of_record`).
pub fn dsn_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".forgefleet").join(DSN_OF_RECORD_CACHE_FILE))
}

/// Best-effort write of the node-local DSN cache. Never errors — caching is an
/// optimization, not a correctness requirement.
pub fn write_dsn_cache(dsn: &str) {
    let Some(path) = dsn_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, dsn.trim());
}

/// Best-effort read of the node-local DSN cache. `None` when absent/empty.
pub fn read_dsn_cache() -> Option<String> {
    let path = dsn_cache_path()?;
    let trimmed = std::fs::read_to_string(&path).ok()?.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Options for pool creation beyond what's in `DatabaseConfig`.
#[derive(Debug, Clone)]
pub struct PoolOptions {
    /// Minimum idle connections to keep warm.
    pub min_connections: u32,
    /// Max lifetime of a connection before it's recycled.
    pub max_lifetime: Duration,
    /// Idle timeout before a connection is closed.
    pub idle_timeout: Duration,
    /// Timeout for acquiring a connection from the pool.
    pub acquire_timeout: Duration,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            min_connections: 1,
            max_lifetime: Duration::from_secs(30 * 60), // 30 minutes
            idle_timeout: Duration::from_secs(10 * 60), // 10 minutes
            acquire_timeout: Duration::from_secs(10),
        }
    }
}

/// Create a Postgres connection pool from the fleet config.
///
/// This parses the URL, sets pool parameters, and returns a connected pool.
/// Does NOT run migrations — that's the caller's responsibility.
pub async fn create_pool(db_config: &DatabaseConfig) -> Result<PgPool> {
    create_pool_with_options(db_config, &PoolOptions::default()).await
}

/// Create a Postgres connection pool with custom pool options.
pub async fn create_pool_with_options(
    db_config: &DatabaseConfig,
    opts: &PoolOptions,
) -> Result<PgPool> {
    let connect_options: PgConnectOptions = db_config
        .url
        .parse()
        .map_err(|e: sqlx::Error| ForgeFleetError::Database(e))?;

    let pool = PgPoolOptions::new()
        .max_connections(db_config.max_connections)
        .min_connections(opts.min_connections)
        .max_lifetime(opts.max_lifetime)
        .idle_timeout(opts.idle_timeout)
        .acquire_timeout(opts.acquire_timeout)
        .connect_with(connect_options)
        .await?;

    info!(
        max_conn = db_config.max_connections,
        "postgres pool created"
    );
    Ok(pool)
}

/// Check if the database is reachable with a simple query.
pub async fn health_check(pool: &PgPool) -> Result<()> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map_err(ForgeFleetError::Database)?;
    Ok(())
}

/// Gracefully close the pool, waiting for active queries to finish.
pub async fn shutdown_pool(pool: PgPool) {
    info!("shutting down postgres pool…");
    pool.close().await;
    info!("postgres pool closed");
}

/// Connect a pool from a raw URL with the daemon's standard pool sizing.
/// Shared by [`create_pool`] and the DSN-failover retry so both paths produce
/// an identically-configured pool.
async fn connect_url(url: &str, max_connections: u32) -> std::result::Result<PgPool, sqlx::Error> {
    let connect_options: PgConnectOptions = url.parse()?;
    PgPoolOptions::new()
        .max_connections(max_connections.max(2))
        .min_connections(0)
        .idle_timeout(Some(Duration::from_secs(60)))
        .max_lifetime(Some(Duration::from_secs(30 * 60)))
        .connect_with(connect_options)
        .await
}

/// Build the worker's Postgres pool, honouring the opt-in HA Phase 3
/// "DSN of record" failover.
///
/// 1. Always try the STATIC [`DatabaseConfig::url`] first.
/// 2. If that fails AND `db_config.dsn_failover` is `true`, read the
///    last-known DSN of record from the node-local cache ([`read_dsn_cache`])
///    and retry against it.
/// 3. If the flag is off, or no cache entry exists, or the cached DSN matches
///    the static one, the ORIGINAL static-DSN error is returned unchanged.
///
/// This is the only DSN-repoint touchpoint on the connect path, and it is
/// fail-safe: with the default `dsn_failover = false` it is exactly equivalent
/// to a single `connect_url(static)`.
pub async fn create_pool_with_dsn_failover(db_config: &DatabaseConfig) -> Result<PgPool> {
    let static_url = db_config.url.trim();
    let static_err = match connect_url(static_url, db_config.max_connections).await {
        Ok(pool) => return Ok(pool),
        Err(e) => e,
    };

    if !db_config.dsn_failover {
        return Err(ForgeFleetError::Database(static_err));
    }

    // Static DSN dead + failover opted-in: consult the DSN of record.
    let Some(record) = read_dsn_cache() else {
        warn!(
            "dsn-failover: static DSN unreachable and no DSN-of-record cache present — \
             returning original error"
        );
        return Err(ForgeFleetError::Database(static_err));
    };
    if record.trim() == static_url {
        // Nothing new to try.
        return Err(ForgeFleetError::Database(static_err));
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
            Err(ForgeFleetError::Database(static_err))
        }
    }
}

/// Try to create a pool, but return `None` instead of failing if
/// the database is unreachable.  Useful during startup when Postgres
/// may not be running yet on every node.
pub async fn try_create_pool(db_config: &DatabaseConfig) -> Option<PgPool> {
    match create_pool(db_config).await {
        Ok(pool) => {
            match health_check(&pool).await {
                Ok(()) => {
                    info!("postgres pool healthy");
                    Some(pool)
                }
                Err(e) => {
                    warn!(error = %e, "postgres health check failed — pool created but DB unreachable");
                    Some(pool) // pool exists, DB might come up later
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "could not create postgres pool — running without DB");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_shape() {
        if let Some(p) = dsn_cache_path() {
            assert!(p.ends_with(DSN_OF_RECORD_CACHE_FILE));
            assert!(p.to_string_lossy().contains(".forgefleet"));
        }
    }

    #[test]
    fn cache_roundtrip_and_empty() {
        let tmp = std::env::temp_dir().join(format!("ffdsn-core-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let prev = std::env::var_os("HOME");
        // SAFETY: single-threaded unit test; no concurrent env access.
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        // No file yet → None.
        assert_eq!(read_dsn_cache(), None);

        // Write then read back (trimmed).
        write_dsn_cache("  postgres://u:p@10.0.0.9:55432/forgefleet\n");
        assert_eq!(
            read_dsn_cache().as_deref(),
            Some("postgres://u:p@10.0.0.9:55432/forgefleet")
        );

        // An all-whitespace cache reads as None (fail-safe to static DSN).
        write_dsn_cache("   ");
        assert_eq!(read_dsn_cache(), None);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
