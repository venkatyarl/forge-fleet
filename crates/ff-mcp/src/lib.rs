//! `ff-mcp` — ForgeFleet MCP (Model Context Protocol) server.
//!
//! Implements the MCP specification over JSON-RPC 2.0, exposing ForgeFleet
//! tools to AI assistants like OpenClaw, Claude, and others.
//!
//! # Transports
//!
//! - **Stdio** — read JSON-RPC requests line-by-line from stdin, write responses to stdout.
//! - **HTTP** — axum-based `POST /mcp` endpoint for networked access.
//!
//! # Architecture
//!
//! ```text
//! Transport (stdio / HTTP)
//!   → McpServer (routes method to handler)
//!     → handlers (fleet_status, fleet_ssh, …)
//!       → integrated ForgeFleet crates (ff-core, ff-discovery, ff-ssh, ff-api, ff-runtime)
//! ```

pub mod brain_tools;
pub mod federation;
pub mod handlers;
pub mod protocol;
pub mod server;
pub mod tools;
pub mod transport;

pub use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use server::McpServer;
pub use tools::ToolRegistry;

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
