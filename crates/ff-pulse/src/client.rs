//! PulseClient — core Redis client wrapper for Fleet Pulse.
//!
//! All Redis key operations use the pattern `{prefix}:{node}:metrics` with a
//! 30-second TTL. Disappearing keys == offline nodes.

use chrono::Utc;
use redis::AsyncCommands;
use tracing::{debug, warn};

use crate::error::Result;
use crate::metrics::{FleetSnapshot, NodeMetrics, PulseEvent};

/// Core Redis client for Fleet Pulse operations.
pub struct PulseClient {
    conn: redis::aio::ConnectionManager,
    prefix: String,
}

/// TTL for node metrics keys (seconds). If a node misses two heartbeats
/// (15s interval), its key expires and it's considered offline.
const METRICS_TTL_SECS: u64 = 30;

impl PulseClient {
    /// Connect to Redis and return a new PulseClient.
    pub async fn connect(redis_url: &str) -> Result<Self> {
        Self::connect_with_prefix(redis_url, "pulse").await
    }

    /// Connect with a custom key prefix (useful for testing).
    pub async fn connect_with_prefix(redis_url: &str, prefix: &str) -> Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = redis::aio::ConnectionManager::new(client).await?;
        debug!("Connected to Redis at {redis_url} with prefix '{prefix}'");
        Ok(Self {
            conn,
            prefix: prefix.to_string(),
        })
    }

    /// Publish node metrics to Redis with TTL and notify subscribers.
    ///
    /// Sets `pulse:{node}:metrics` with a 30s TTL and publishes to
    /// the `pulse:updates` channel for real-time dashboard push.
    pub async fn publish_metrics(&mut self, node: &str, metrics: &NodeMetrics) -> Result<()> {
        let key = format!("{}:{}:metrics", self.prefix, node);
        let channel = format!("{}:updates", self.prefix);
        let json = serde_json::to_string(metrics)?;

        // SET with TTL
        self.conn
            .set_ex::<_, _, ()>(&key, &json, METRICS_TTL_SECS)
            .await?;

        // Publish for real-time subscribers
        self.conn.publish::<_, _, ()>(&channel, &json).await?;

        debug!("Published metrics for node '{node}'");
        Ok(())
    }

    /// Retrieve the latest metrics for a specific node.
    pub async fn get_metrics(&mut self, node: &str) -> Result<Option<NodeMetrics>> {
        let key = format!("{}:{}:metrics", self.prefix, node);
        let value: Option<String> = self.conn.get(&key).await?;
        match value {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Retrieve metrics for all online nodes.
    pub async fn get_all_metrics(&mut self) -> Result<FleetSnapshot> {
        let pattern = format!("{}:*:metrics", self.prefix);
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(&pattern)
            .query_async(&mut self.conn)
            .await?;

        let mut nodes = Vec::with_capacity(keys.len());
        for key in &keys {
            let value: Option<String> = self.conn.get(key).await?;
            if let Some(json) = value {
                match serde_json::from_str::<NodeMetrics>(&json) {
                    Ok(m) => nodes.push(m),
                    Err(e) => warn!("Failed to parse metrics from key '{key}': {e}"),
                }
            }
        }

        let total_ram_gb: f64 = nodes.iter().map(|n| n.ram_total_gb).sum();
        let total_tokens_per_sec: f64 = nodes.iter().map(|n| n.tokens_per_sec).sum();

        Ok(FleetSnapshot {
            timestamp: Utc::now(),
            nodes: nodes.clone(),
            online_count: nodes.len(),
            total_ram_gb,
            total_tokens_per_sec,
        })
    }

    /// Check whether a node is alive (its metrics key exists and hasn't expired).
    pub async fn is_node_alive(&mut self, node: &str) -> Result<bool> {
        let key = format!("{}:{}:metrics", self.prefix, node);
        let exists: bool = self.conn.exists(&key).await?;
        Ok(exists)
    }

    /// List all nodes that currently have live metrics keys.
    pub async fn list_online_nodes(&mut self) -> Result<Vec<String>> {
        let pattern = format!("{}:*:metrics", self.prefix);
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(&pattern)
            .query_async(&mut self.conn)
            .await?;

        // Extract node names from keys like "pulse:taylor:metrics"
        let prefix_len = self.prefix.len() + 1; // "pulse:"
        let suffix = ":metrics";
        let nodes = keys
            .into_iter()
            .filter_map(|k| {
                if k.len() > prefix_len + suffix.len() {
                    Some(k[prefix_len..k.len() - suffix.len()].to_string())
                } else {
                    None
                }
            })
            .collect();

        Ok(nodes)
    }

    /// Publish a fleet event on the pulse:events channel.
    pub async fn publish_event(&mut self, event: &PulseEvent) -> Result<()> {
        let channel = format!("{}:events", self.prefix);
        let json = serde_json::to_string(event)?;
        self.conn.publish::<_, _, ()>(&channel, &json).await?;
        debug!("Published event {:?} for node '{}'", event.event_type, event.node_name);
        Ok(())
    }
}
