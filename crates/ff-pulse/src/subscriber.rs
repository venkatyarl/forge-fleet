//! PulseSubscriber — subscribe to real-time Redis pub/sub channels
//! for dashboard updates and fleet events.

use futures::Stream;
use redis::Client;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, warn};

use crate::error::Result;
use crate::metrics::{NodeMetrics, PulseEvent};

/// Subscribes to Fleet Pulse Redis pub/sub channels.
pub struct PulseSubscriber {
    client: Client,
    prefix: String,
}

impl PulseSubscriber {
    /// Create a new subscriber connected to the given Redis URL.
    pub fn new(redis_url: &str) -> Result<Self> {
        Self::new_with_prefix(redis_url, "pulse")
    }

    /// Create a subscriber with a custom prefix.
    pub fn new_with_prefix(redis_url: &str, prefix: &str) -> Result<Self> {
        let client = Client::open(redis_url)?;
        Ok(Self {
            client,
            prefix: prefix.to_string(),
        })
    }

    /// Subscribe to the `pulse:updates` channel and receive `NodeMetrics`
    /// as an async stream.
    pub fn subscribe_updates(&self) -> Result<impl Stream<Item = NodeMetrics>> {
        let channel = format!("{}:updates", self.prefix);
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel::<NodeMetrics>(128);

        tokio::spawn(async move {
            let conn = match client.get_async_pubsub().await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to open pub/sub connection: {e}");
                    return;
                }
            };
            let mut pubsub = conn;
            if let Err(e) = pubsub.subscribe(&channel).await {
                error!("Failed to subscribe to '{channel}': {e}");
                return;
            }
            debug!("Subscribed to '{channel}'");

            let mut msg_stream = pubsub.into_on_message();
            use futures::StreamExt;
            while let Some(msg) = msg_stream.next().await {
                let payload: String = match msg.get_payload() {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Invalid message payload: {e}");
                        continue;
                    }
                };
                match serde_json::from_str::<NodeMetrics>(&payload) {
                    Ok(metrics) => {
                        if tx.send(metrics).await.is_err() {
                            debug!("Subscriber receiver dropped, stopping");
                            break;
                        }
                    }
                    Err(e) => warn!("Failed to parse NodeMetrics: {e}"),
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }

    /// Subscribe to the `pulse:events` channel and receive `PulseEvent`
    /// as an async stream.
    pub fn subscribe_events(&self) -> Result<impl Stream<Item = PulseEvent>> {
        let channel = format!("{}:events", self.prefix);
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel::<PulseEvent>(128);

        tokio::spawn(async move {
            let conn = match client.get_async_pubsub().await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to open pub/sub connection: {e}");
                    return;
                }
            };
            let mut pubsub = conn;
            if let Err(e) = pubsub.subscribe(&channel).await {
                error!("Failed to subscribe to '{channel}': {e}");
                return;
            }
            debug!("Subscribed to '{channel}'");

            let mut msg_stream = pubsub.into_on_message();
            use futures::StreamExt;
            while let Some(msg) = msg_stream.next().await {
                let payload: String = match msg.get_payload() {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Invalid message payload: {e}");
                        continue;
                    }
                };
                match serde_json::from_str::<PulseEvent>(&payload) {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            debug!("Subscriber receiver dropped, stopping");
                            break;
                        }
                    }
                    Err(e) => warn!("Failed to parse PulseEvent: {e}"),
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}
