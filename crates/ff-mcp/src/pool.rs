//! Process-global cached connection pools for the MCP server.
//!
//! Building a fresh `PgPool` (or Redis `ConnectionManager`) on **every** tool
//! call is the documented pool-per-call anti-pattern that exhausts Postgres
//! connections under load — it caused two Vinny outages (2026-04-23). Before
//! this module, `brain_tools`, `cortex_tools`, and several `handlers` paths each
//! called `PgPoolOptions::new().connect()` per invocation, and the fleet-status
//! handlers opened a new `PulseClient` per call.
//!
//! Both `sqlx::PgPool` and the pulse `ConnectionManager` are `Arc`-backed, so we
//! build each ONCE (lazily, on first use) and hand out cheap clones that share
//! the same underlying connections.

use std::time::Duration;

use ff_core::config;
use ff_pulse::PulseClient;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::OnceCell;

/// Max connections for the single shared MCP pool. One pool now backs every
/// tool call, so this is sized for concurrent MCP traffic rather than the
/// old per-call `max_connections(2)` that multiplied without bound.
const MCP_POOL_MAX_CONNECTIONS: u32 = 8;
const MCP_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MCP_POOL_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
const MCP_POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

static PG_POOL: OnceCell<sqlx::PgPool> = OnceCell::const_new();
static PULSE: OnceCell<PulseClient> = OnceCell::const_new();

fn pg_pool_options() -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(MCP_POOL_MAX_CONNECTIONS)
        .min_connections(0)
        .idle_timeout(Some(MCP_POOL_IDLE_TIMEOUT))
        .max_lifetime(Some(MCP_POOL_MAX_LIFETIME))
        .acquire_timeout(MCP_POOL_ACQUIRE_TIMEOUT)
}

/// The shared MCP Postgres pool, built once from the fleet config. Returns a
/// cheap clone (the pool is an `Arc` internally; clones share connections).
pub async fn shared_pg_pool() -> Result<sqlx::PgPool, String> {
    PG_POOL
        .get_or_try_init(|| async {
            let (cfg, _) = config::load_config_auto()
                .map_err(|e| format!("failed to load fleet config: {e}"))?;
            pg_pool_options()
                .connect(&cfg.database.url)
                .await
                .map_err(|e| format!("Postgres connection failed: {e}"))
        })
        .await
        .cloned()
}

/// The shared MCP pulse (Redis) client, built once from the fleet config.
/// Returns a clone sharing the same auto-reconnecting `ConnectionManager`.
pub async fn shared_pulse() -> Result<PulseClient, String> {
    PULSE
        .get_or_try_init(|| async {
            let (cfg, _) = config::load_config_auto()
                .map_err(|e| format!("failed to load fleet config: {e}"))?;
            PulseClient::connect(&cfg.redis.url)
                .await
                .map_err(|e| format!("Redis connection failed: {e}"))
        })
        .await
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_pool_has_bounded_idle_and_acquire_policy() {
        let options = pg_pool_options();

        assert_eq!(options.get_max_connections(), MCP_POOL_MAX_CONNECTIONS);
        assert_eq!(options.get_min_connections(), 0);
        assert_eq!(options.get_idle_timeout(), Some(MCP_POOL_IDLE_TIMEOUT));
        assert_eq!(options.get_max_lifetime(), Some(MCP_POOL_MAX_LIFETIME));
        assert_eq!(options.get_acquire_timeout(), MCP_POOL_ACQUIRE_TIMEOUT);
    }
}
