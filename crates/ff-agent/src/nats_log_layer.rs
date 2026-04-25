//! NATS log forwarding layer.
//!
//! A `tracing_subscriber::Layer` that mirrors every emitted event to NATS
//! on subject `logs.{computer}.{service}.{level}`. Fire-and-forget: each
//! event is serialized to JSON and published via `tokio::spawn` so the
//! logging hot-path never waits on network I/O.
//!
//! NATS is optional infrastructure — construct with [`NatsLogLayer::new`],
//! which connects at startup. If the connection fails, callers should log
//! a warning and skip wiring the layer; the file + stdout layers continue
//! to work unchanged.

use std::collections::BTreeMap;
use std::fmt::Debug;

use async_nats::Client;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;

#[derive(Debug, Error)]
pub enum NatsError {
    #[error("nats connect: {0}")]
    Connect(#[from] async_nats::ConnectError),
}

/// Tracing layer that mirrors log events to NATS.
pub struct NatsLogLayer {
    client: Client,
    computer_name: String,
    service: String,
}

impl NatsLogLayer {
    /// Build a new layer. Connects to NATS at `nats_url` and tags each
    /// published event with `computer_name` and `service`.
    pub async fn new(
        nats_url: &str,
        computer_name: String,
        service: String,
    ) -> Result<Self, NatsError> {
        let client = async_nats::connect(nats_url).await?;
        Ok(Self {
            client,
            computer_name,
            service,
        })
    }

    /// Build using an already-connected client (reuses the process-global
    /// NATS connection from `nats_client`).
    pub fn with_client(client: Client, computer_name: String, service: String) -> Self {
        Self {
            client,
            computer_name,
            service,
        }
    }
}

/// Visitor that flattens a tracing event's fields into a JSON map.
struct JsonVisitor<'a>(&'a mut BTreeMap<String, Value>);

impl<'a> Visit for JsonVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0
            .insert(field.name().to_string(), json!(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
}

impl<S> Layer<S> for NatsLogLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let level = metadata.level().to_string().to_lowercase();
        let target = metadata.target().to_string();

        let mut fields: BTreeMap<String, Value> = BTreeMap::new();
        event.record(&mut JsonVisitor(&mut fields));

        // Extract the message if present; keep other fields in a "fields"
        // sub-object to avoid collisions with our own wrapper keys.
        let message = fields
            .remove("message")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let payload = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "level": level,
            "target": target,
            "computer": self.computer_name,
            "service": self.service,
            "message": message,
            "fields": fields,
        });

        let bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => return,
        };

        let subject = format!("logs.{}.{}.{}", self.computer_name, self.service, level);

        // Fire-and-forget. Clone the client handle (cheap — it's an Arc inside).
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.publish(subject, bytes.into()).await;
        });
    }
}
