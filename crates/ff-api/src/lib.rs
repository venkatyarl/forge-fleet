pub mod adaptive_router;
pub mod classifier;
pub mod config;
pub mod error;
pub mod openai_compat;
pub mod quality_tracker;
pub mod registry;
pub mod router;
pub mod server;
pub mod tool_calling;
pub mod types;

use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::info;

use config::ApiConfig;
use registry::BackendRegistry;
use server::{AppState, build_http_router};

pub async fn run(config: ApiConfig) -> anyhow::Result<()> {
    let bind_addr = config.bind_addr();
    let registry = Arc::new(BackendRegistry::new(config.backends));
    let state = Arc::new(AppState::new(registry)?);

    let app = build_http_router(state);
    let listener = TcpListener::bind(bind_addr).await?;
    info!(address = %listener.local_addr()?, "ff-api listening");

    axum::serve(listener, app).await?;
    Ok(())
}
