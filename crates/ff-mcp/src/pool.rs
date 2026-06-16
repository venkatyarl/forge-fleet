//! Process-global cached connection pools for the MCP server.
//!
//! Building a fresh `PgPool` (or Redis `ConnectionManager`) on **every** tool
//! call is the documented pool-per-call anti-pattern that exhausts Postgres
//! connections under load — it caused two Taylor outages (2026-04-23). Before
//! this module, `brain_tools`, `cortex_tools`, and several `handlers` paths each
//! called `PgPoolOptions::new().connect()` per invocation, and the fleet-status
//! handlers opened a new `PulseClient` per call.
//!
//! Both `sqlx::PgPool` and the pulse `ConnectionManager` are `Arc`-backed, so we
//! build each ONCE (lazily, on first use) and hand out cheap clones that share
//! the same underlying connections.

use ff_core::config;
use ff_pulse::PulseClient;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::OnceCell;

/// Max connections for the single shared MCP pool. One pool now backs every
/// tool call, so this is sized for concurrent MCP traffic rather than the
/// old per-call `max_connections(2)` that multiplied without bound.
const MCP_POOL_MAX_CONNECTIONS: u32 = 8;

static PG_POOL: OnceCell<sqlx::PgPool> = OnceCell::const_new();
static PULSE: OnceCell<PulseClient> = OnceCell::const_new();

/// The shared MCP Postgres pool, built once from the fleet config. Returns a
/// cheap clone (the pool is an `Arc` internally; clones share connections).
pub async fn shared_pg_pool() -> Result<sqlx::PgPool, String> {
    PG_POOL
        .get_or_try_init(|| async {
            let (cfg, _) = config::load_config_auto()
                .map_err(|e| format!("failed to load fleet config: {e}"))?;
            PgPoolOptions::new()
                .max_connections(MCP_POOL_MAX_CONNECTIONS)
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
