//! Postgres connection pool setup with sqlx.
//!
//! Provides a thin wrapper around `sqlx::PgPool` with config-driven setup,
//! health checks, and graceful shutdown.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing::{info, warn};

use crate::config::DatabaseConfig;
use crate::error::{ForgeFleetError, Result};

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
