//! `ff-gateway` — ForgeFleet multi-channel messaging gateway.
//!
//! This crate provides:
//! - **server** — Axum HTTP server with WebSocket chat endpoint and dashboard
//! - **telegram** — Telegram Bot API integration (webhook/polling, buttons, reactions, media)
//! - **discord** — Discord REST integration (messages, reactions, threads)
//! - **webhook** — Generic webhook normalization into internal message format
//! - **message** — Shared incoming/outgoing message types and media/reaction models
//! - **router** — Message routing for chat, command, and tool-execution flows
//! - **embed** — Embeddable web widget JavaScript endpoint

pub mod discord;
pub mod embed;
pub mod brain_api;
pub mod llm_routing;
pub mod message;
pub mod middleware;
pub mod onboard;
pub mod pulse_api;
pub mod router;
pub mod server;
pub mod static_files;
pub mod telegram;
pub mod telegram_transport;
pub mod webhook;
pub mod websocket;

pub use discord::{DiscordClient, DiscordError};
pub use embed::{EmbedConfig, build_widget_script};
pub use message::{
    Channel, IncomingMessage, MessageButton, MessageMedia, MessageMediaKind, OutgoingMessage,
    ParsedCommand, Reaction, ReactionAction,
};
pub use router::{MessageRouter, RouteTarget, RoutedMessage, ToolExecutionIntent};
pub use server::{GatewayConfig, GatewayServer, GatewayState, build_router, run};
pub use telegram::{TelegramClient, TelegramError};
pub use telegram_transport::TelegramPollingTransport;
pub use webhook::{WebhookAcceptedResponse, WebhookError, normalize_payload};
pub use websocket::{EventType, WsHub};
