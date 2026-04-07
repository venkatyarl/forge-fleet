//! WebSocket hub for live dashboard updates.
//!
//! Maintains a set of connected clients, supports topic subscriptions,
//! broadcasts events, and runs a heartbeat to prune dead connections.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ─── Event types ─────────────────────────────────────────────────────────────

/// Event categories that clients can subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// Node came online / went offline / health changed
    NodeStatus,
    /// Task started / completed / failed
    TaskUpdate,
    /// fleet.toml or config reloaded
    ConfigReload,
    /// Incoming or outgoing chat message
    Message,
    /// Agent loop events (tool start/end, thinking, status, done)
    AgentEvent,
    /// Any event (wildcard)
    All,
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeStatus => write!(f, "node_status"),
            Self::TaskUpdate => write!(f, "task_update"),
            Self::ConfigReload => write!(f, "config_reload"),
            Self::Message => write!(f, "message"),
            Self::AgentEvent => write!(f, "agent_event"),
            Self::All => write!(f, "all"),
        }
    }
}

// ─── Client entry ────────────────────────────────────────────────────────────

struct WsClient {
    sender: mpsc::UnboundedSender<Message>,
    subscriptions: HashSet<EventType>,
    last_pong: Instant,
}

// ─── Hub ─────────────────────────────────────────────────────────────────────

/// Central WebSocket connection manager.
///
/// Thread-safe (`Clone + Send + Sync`) — share via `Arc` or clone directly.
#[derive(Clone)]
pub struct WsHub {
    clients: Arc<DashMap<Uuid, WsClient>>,
}

impl WsHub {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
        }
    }

    /// Register a new client.  Returns `(session_id, receiver)`.
    ///
    /// If `subscriptions` is `None`, the client subscribes to `All`.
    pub fn register(
        &self,
        subscriptions: Option<HashSet<EventType>>,
    ) -> (Uuid, mpsc::UnboundedReceiver<Message>) {
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::unbounded_channel();

        let subs = subscriptions.unwrap_or_else(|| {
            let mut s = HashSet::new();
            s.insert(EventType::All);
            s
        });

        self.clients.insert(
            id,
            WsClient {
                sender: tx,
                subscriptions: subs,
                last_pong: Instant::now(),
            },
        );

        info!(session = %id, "ws client registered");
        (id, rx)
    }

    /// Remove a client from the hub.
    pub fn unregister(&self, id: &Uuid) {
        self.clients.remove(id);
        info!(session = %id, "ws client unregistered");
    }

    /// Number of connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Update a client's subscriptions.
    pub fn set_subscriptions(&self, id: &Uuid, subs: HashSet<EventType>) {
        if let Some(mut entry) = self.clients.get_mut(id) {
            entry.subscriptions = subs;
            debug!(session = %id, "subscriptions updated");
        }
    }

    /// Record a pong from a client (resets heartbeat timer).
    pub fn record_pong(&self, id: &Uuid) {
        if let Some(mut entry) = self.clients.get_mut(id) {
            entry.last_pong = Instant::now();
        }
    }

    // ── Broadcasting ─────────────────────────────────────────────────────

    /// Broadcast a raw JSON value to ALL connected clients (ignoring subscriptions).
    pub fn broadcast_raw(&self, payload: Value) {
        let text = payload.to_string();
        let mut dead = Vec::new();

        for entry in self.clients.iter() {
            if entry
                .sender
                .send(Message::Text(text.clone().into()))
                .is_err()
            {
                dead.push(*entry.key());
            }
        }

        for id in dead {
            self.clients.remove(&id);
        }
    }

    /// Broadcast an event to clients subscribed to `event_type`.
    pub fn broadcast_event(&self, event_type: EventType, payload: Value) {
        let envelope = json!({
            "type": event_type.to_string(),
            "data": payload,
            "ts": chrono::Utc::now().to_rfc3339(),
        });
        let text = envelope.to_string();
        let mut dead = Vec::new();

        for entry in self.clients.iter() {
            let subscribed = entry.subscriptions.contains(&EventType::All)
                || entry.subscriptions.contains(&event_type);

            if subscribed
                && entry
                    .sender
                    .send(Message::Text(text.clone().into()))
                    .is_err()
            {
                dead.push(*entry.key());
            }
        }

        for id in dead {
            self.clients.remove(&id);
        }
    }

    // ── Heartbeat ────────────────────────────────────────────────────────

    /// Spawn a background task that pings all clients every `interval` and
    /// drops any client that hasn't responded within `timeout`.
    pub fn spawn_heartbeat_task(
        &self,
        interval: Duration,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let clients = self.clients.clone();

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tick.tick().await;
                let now = Instant::now();
                let mut dead = Vec::new();

                for entry in clients.iter() {
                    // Prune clients whose last pong exceeds timeout
                    if now.duration_since(entry.last_pong) > timeout {
                        warn!(session = %entry.key(), "ws client timed out — disconnecting");
                        dead.push(*entry.key());
                        continue;
                    }

                    // Send a ping
                    if entry.sender.send(Message::Ping(Vec::new().into())).is_err() {
                        dead.push(*entry.key());
                    }
                }

                for id in dead {
                    clients.remove(&id);
                }
            }
        })
    }
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Inbound command parsing ─────────────────────────────────────────────────

/// A command sent by a WS client (e.g. subscribe).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsCommand {
    Subscribe { events: Vec<EventType> },
    Unsubscribe { events: Vec<EventType> },
    Ping,
}

/// Try to parse a WS text frame as a hub command.  Returns `Some(cmd)` if
/// the `"type"` field matched a known command.
pub fn try_parse_command(text: &str) -> Option<WsCommand> {
    serde_json::from_str::<WsCommand>(text).ok()
}
