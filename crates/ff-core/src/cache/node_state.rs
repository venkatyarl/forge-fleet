//! Redis-backed cache for volatile fleet node state.
//!
//! Current load and session bindings are high-churn, freshness-only data —
//! exactly the class of state the audit in
//! `docs/audits/audit-ephemeral-live-state-data-to-redis.md` calls out as
//! Redis-authoritative rather than Postgres-durable. A missing/expired key
//! means "unknown", never "offline"; callers must treat cache misses as such.
//!
//! Follows the same `ConnectionManager` + JSON + `SET EX` pattern as
//! `ff_pulse::client::PulseClient`.

use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::Result;

/// TTL for node load keys (seconds). Load is reported on a short heartbeat
/// cadence; a missing key means the node hasn't reported recently.
const NODE_LOAD_TTL_SECS: u64 = 30;

/// TTL for session binding keys (seconds). Bindings outlive a single
/// heartbeat but should not survive an idle session indefinitely.
const SESSION_BINDING_TTL_SECS: u64 = 300;

/// Current load snapshot for a fleet node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLoad {
    pub active_tasks: u32,
    pub cpu_pct: f32,
    pub ram_used_gb: f32,
    pub updated_at: DateTime<Utc>,
}

/// Which node a session is currently bound to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBinding {
    pub node: String,
    pub bound_at: DateTime<Utc>,
}

/// Redis-backed cache for volatile fleet node state.
///
/// `Clone` is cheap: `ConnectionManager` is `Arc`-backed and auto-reconnecting,
/// so clones share one underlying connection.
#[derive(Clone)]
pub struct NodeStateCache {
    conn: redis::aio::ConnectionManager,
    prefix: String,
}

impl NodeStateCache {
    /// Connect to Redis and return a new `NodeStateCache` using the `fleet` prefix.
    pub async fn connect(redis_url: &str) -> Result<Self> {
        Self::connect_with_prefix(redis_url, "fleet").await
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

    /// Set the current load snapshot for `node`, with a short TTL.
    pub async fn set_node_load(&mut self, node: &str, load: &NodeLoad) -> Result<()> {
        let key = node_load_key(&self.prefix, node);
        let json = serde_json::to_string(load)?;
        self.conn
            .set_ex::<_, _, ()>(&key, json, NODE_LOAD_TTL_SECS)
            .await?;
        Ok(())
    }

    /// Get the current load snapshot for `node`, if it has reported recently.
    pub async fn get_node_load(&mut self, node: &str) -> Result<Option<NodeLoad>> {
        let key = node_load_key(&self.prefix, node);
        let value: Option<String> = self.conn.get(&key).await?;
        value
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(Into::into)
    }

    /// Bind a session to `node`, with a longer-lived TTL than node load.
    pub async fn set_session_binding(
        &mut self,
        session_id: &str,
        binding: &SessionBinding,
    ) -> Result<()> {
        let key = session_binding_key(&self.prefix, session_id);
        let json = serde_json::to_string(binding)?;
        self.conn
            .set_ex::<_, _, ()>(&key, json, SESSION_BINDING_TTL_SECS)
            .await?;
        Ok(())
    }

    /// Get the node a session is currently bound to, if the binding hasn't expired.
    pub async fn get_session_binding(
        &mut self,
        session_id: &str,
    ) -> Result<Option<SessionBinding>> {
        let key = session_binding_key(&self.prefix, session_id);
        let value: Option<String> = self.conn.get(&key).await?;
        value
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(Into::into)
    }
}

fn node_load_key(prefix: &str, node: &str) -> String {
    format!("{prefix}:node:{node}:load")
}

fn session_binding_key(prefix: &str, session_id: &str) -> String {
    format!("{prefix}:session:{session_id}:binding")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_naming_uses_prefix_and_kind() {
        assert_eq!(node_load_key("fleet", "mac-1"), "fleet:node:mac-1:load");
        assert_eq!(
            session_binding_key("fleet", "sess-abc"),
            "fleet:session:sess-abc:binding"
        );
    }
}
