//! MCP transports — stdio and HTTP.
//!
//! # Stdio Transport
//! Reads JSON-RPC requests line-by-line from stdin, processes them through
//! `McpServer`, and writes responses to stdout. One request per line (NDJSON).
//!
//! # HTTP Transport
//! Axum-based HTTP server exposing a `POST /mcp` endpoint that accepts
//! JSON-RPC requests in the body and returns JSON-RPC responses.

use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::Value;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::server::McpServer;

// ─── Stdio Transport ─────────────────────────────────────────────────────────

/// Stdio-based MCP transport.
///
/// Reads one JSON-RPC request per line from stdin, dispatches through
/// `McpServer`, and writes the response as a single line to stdout.
pub struct StdioTransport {
    server: McpServer,
}

impl StdioTransport {
    pub fn new(server: McpServer) -> Self {
        Self { server }
    }

    /// Run the stdio transport loop. Blocks until stdin is closed.
    pub async fn run(&self) -> anyhow::Result<()> {
        info!("MCP stdio transport starting");

        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            debug!(len = line.len(), "received stdin line");

            // Parse JSON-RPC request
            let request: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(e) => {
                    warn!("failed to parse JSON-RPC request: {e}");
                    let error_resp =
                        JsonRpcResponse::error(None, JsonRpcError::parse_error(e.to_string()));
                    let json = serde_json::to_string(&error_resp)?;
                    stdout.write_all(json.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    continue;
                }
            };

            // Dispatch to server
            if let Some(response) = self.server.handle_request(request).await {
                let json = serde_json::to_string(&response)?;
                stdout.write_all(json.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
            // If None (notification), no response is sent
        }

        info!("MCP stdio transport shutting down (stdin closed)");
        Ok(())
    }
}

// ─── HTTP Transport ──────────────────────────────────────────────────────────

/// HTTP-based MCP transport using axum.
///
/// Exposes a `POST /mcp` endpoint that accepts JSON-RPC requests.
pub struct HttpTransport {
    server: Arc<McpServer>,
}

impl HttpTransport {
    pub fn new(server: McpServer) -> Self {
        Self {
            server: Arc::new(server),
        }
    }

    /// Build the axum router.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/mcp", post(handle_mcp_post))
            .route("/mcp/health", axum::routing::get(handle_health))
            .with_state(self.server.clone())
    }

    /// Start the HTTP server on the given address.
    pub async fn run(&self, addr: &str) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!(addr, "MCP HTTP transport listening");

        axum::serve(listener, self.router()).await?;

        Ok(())
    }
}

/// Axum handler for `POST /mcp`.
async fn handle_mcp_post(
    State(server): State<Arc<McpServer>>,
    Json(request): Json<JsonRpcRequest>,
) -> (StatusCode, Json<Value>) {
    match server.handle_request(request).await {
        Some(response) => (
            StatusCode::OK,
            Json(serde_json::to_value(response).unwrap_or_default()),
        ),
        None => (StatusCode::NO_CONTENT, Json(Value::Null)),
    }
}

/// Health check endpoint.
async fn handle_health() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_builds_router() {
        let server = McpServer::new();
        let transport = HttpTransport::new(server);
        let _router = transport.router(); // Should not panic
    }
}
