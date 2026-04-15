//! Fleet events — Redis pub/sub for instant node online/offline and task
//! dispatch notifications.
//!
//! This replaces polling latency (~15s for deferred-worker ticks) with
//! push-based wake-ups. When the scheduler detects a node transitioning
//! from offline → online, it publishes to `fleet:node_online`. Workers
//! on that node subscribe and immediately claim pending tasks instead
//! of waiting for the next poll tick.
//!
//! Channels:
//! - `fleet:node_online`      — payload = node name (e.g. "ace")
//! - `fleet:node_offline`     — payload = node name
//! - `fleet:task_dispatched`  — payload = JSON `{task_id, target_node}`
//!
//! The Redis URL is resolved from `~/.forgefleet/fleet.toml` `[redis] url`,
//! falling back to `redis://192.168.5.100:6380` if unreadable.

use futures::Stream;
use redis::AsyncCommands;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

pub const CHANNEL_NODE_ONLINE: &str = "fleet:node_online";
pub const CHANNEL_NODE_OFFLINE: &str = "fleet:node_offline";
pub const CHANNEL_TASK_DISPATCHED: &str = "fleet:task_dispatched";

/// Resolve the Redis URL, reading `~/.forgefleet/fleet.toml` `[redis] url`.
/// Falls back to `redis://192.168.5.100:6380` on any error.
fn resolve_redis_url() -> String {
    const FALLBACK: &str = "redis://192.168.5.100:6380";
    let Some(home) = dirs::home_dir() else { return FALLBACK.to_string() };
    let path = home.join(".forgefleet/fleet.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return FALLBACK.to_string() };
    // Parse as generic toml::Value so we don't depend on FleetConfig shape.
    let Ok(val) = toml::from_str::<toml::Value>(&text) else { return FALLBACK.to_string() };
    val.get("redis")
        .and_then(|r| r.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| FALLBACK.to_string())
}

async fn publish_raw(channel: &str, payload: &str) -> Result<(), String> {
    let url = resolve_redis_url();
    let client = redis::Client::open(url.as_str())
        .map_err(|e| format!("redis open {url}: {e}"))?;
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| format!("redis connect {url}: {e}"))?;
    conn.publish::<_, _, ()>(channel, payload)
        .await
        .map_err(|e| format!("redis publish {channel}: {e}"))?;
    debug!(channel, payload, "published fleet event");
    Ok(())
}

/// Publish that `node` transitioned from offline to online.
pub async fn publish_node_online(node: &str) -> Result<(), String> {
    publish_raw(CHANNEL_NODE_ONLINE, node).await
}

/// Publish that `node` transitioned from online to offline.
pub async fn publish_node_offline(node: &str) -> Result<(), String> {
    publish_raw(CHANNEL_NODE_OFFLINE, node).await
}

/// Publish that a task was dispatched, optionally to a specific node.
pub async fn publish_task_dispatched(
    task_id: &str,
    target_node: Option<&str>,
) -> Result<(), String> {
    let payload = serde_json::json!({
        "task_id": task_id,
        "target_node": target_node,
    })
    .to_string();
    publish_raw(CHANNEL_TASK_DISPATCHED, &payload).await
}

/// Simple wrapper turning a tokio mpsc Receiver into a futures Stream,
/// so we don't need to pull in `tokio-stream` as a new dep.
struct McpStream<T> {
    rx: mpsc::Receiver<T>,
}

impl<T> Stream for McpStream<T> {
    type Item = T;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.rx.poll_recv(cx)
    }
}

fn spawn_subscriber<T, F>(channel: &'static str, parse: F) -> impl Stream<Item = T>
where
    T: Send + 'static,
    F: Fn(String) -> Option<T> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<T>(128);
    tokio::spawn(async move {
        let url = resolve_redis_url();
        let client = match redis::Client::open(url.as_str()) {
            Ok(c) => c,
            Err(e) => {
                error!("fleet_events: redis open {url}: {e}");
                return;
            }
        };
        let mut pubsub = match client.get_async_pubsub().await {
            Ok(p) => p,
            Err(e) => {
                error!("fleet_events: redis pubsub {url}: {e}");
                return;
            }
        };
        if let Err(e) = pubsub.subscribe(channel).await {
            error!("fleet_events: subscribe {channel}: {e}");
            return;
        }
        debug!("fleet_events: subscribed to {channel}");
        use futures::StreamExt;
        let mut msgs = pubsub.into_on_message();
        while let Some(msg) = msgs.next().await {
            let payload: String = match msg.get_payload() {
                Ok(p) => p,
                Err(e) => {
                    warn!("fleet_events: invalid payload on {channel}: {e}");
                    continue;
                }
            };
            if let Some(item) = parse(payload) {
                if tx.send(item).await.is_err() {
                    debug!("fleet_events: {channel} receiver dropped");
                    break;
                }
            }
        }
    });
    McpStream { rx }
}

/// Subscribe to `fleet:node_online`. Yields node names.
pub fn subscribe_node_online() -> impl Stream<Item = String> {
    spawn_subscriber(CHANNEL_NODE_ONLINE, Some)
}

/// Subscribe to `fleet:node_offline`. Yields node names.
pub fn subscribe_node_offline() -> impl Stream<Item = String> {
    spawn_subscriber(CHANNEL_NODE_OFFLINE, Some)
}

/// Subscribe to `fleet:task_dispatched`. Yields `(task_id, target_node)`.
pub fn subscribe_task_dispatched() -> impl Stream<Item = (String, Option<String>)> {
    spawn_subscriber(CHANNEL_TASK_DISPATCHED, |payload| {
        let v: serde_json::Value = serde_json::from_str(&payload).ok()?;
        let task_id = v.get("task_id")?.as_str()?.to_string();
        let target_node = v
            .get("target_node")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        Some((task_id, target_node))
    })
}
