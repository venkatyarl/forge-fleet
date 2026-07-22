pub mod adaptive_router;
pub mod autoload;
pub mod circuit_breaker;
pub mod classifier;
pub mod config;
pub mod error;
pub mod openai_compat;
pub mod quality_tracker;
pub mod registry;
pub mod router;
pub mod routes;
pub mod server;
pub mod token_ledger;
pub mod tool_calling;
pub mod types;

use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::{info, warn};

use config::ApiConfig;
use registry::BackendRegistry;
use server::{AppState, build_http_router};

/// Connect the ff-memory store when a Postgres URL is configured; the
/// `/memory/*` routes return 503 without one.
async fn connect_memory_store() -> Option<Arc<ff_memory::MemoryStore>> {
    let url = std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        .ok()?;
    match sqlx::PgPool::connect(&url).await {
        Ok(pool) => Some(Arc::new(ff_memory::MemoryStore::new(pool))),
        Err(error) => {
            warn!(%error, "memory routes disabled: failed to connect to Postgres");
            None
        }
    }
}

pub async fn run(config: ApiConfig) -> anyhow::Result<()> {
    let bind_addr = config.bind_addr();
    let registry = Arc::new(BackendRegistry::new(config.backends));
    let mut state = AppState::new(registry, config.api_keys)?;
    if let Some(store) = connect_memory_store().await {
        state = state.with_memory_store(store);
    }
    let state = Arc::new(state);

    let app = build_http_router(state, &config.cors_allowed_origins);
    let listener = TcpListener::bind(bind_addr).await?;
    info!(address = %listener.local_addr()?, "ff-api listening");

    axum::serve(listener, app).await?;
    Ok(())
}
