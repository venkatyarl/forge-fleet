//! Fleet info — central helper for loading fleet topology from Postgres.
//!
//! ForgeFleet is open-source and must not contain hardcoded fleet identities.
//! All fleet-specific data (node names, IPs, SSH users, model ports) lives in
//! the Postgres `fleet_nodes` and `fleet_models` tables. This module centralizes
//! the query logic so every call site uses the same cached/async API.

use std::time::Duration;

use ff_db::{FleetModelRow, FleetNodeRow, pg_get_node, pg_get_secret, pg_list_models, pg_list_nodes};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::OnceCell;
use tracing::debug;

/// Cached process-wide fleet description used by the default system prompt.
/// Populated on first successful call to [`hydrate_fleet_description`].
static FLEET_DESCRIPTION: OnceCell<String> = OnceCell::const_new();

/// Cached process-wide fleet snapshot (nodes + models) for sync consumers that
/// cannot easily reach Postgres (e.g. the orchestrator's `fleet_capabilities`
/// helper, which is called from legacy sync code paths).
static FLEET_SNAPSHOT: OnceCell<FleetSnapshot> = OnceCell::const_new();

/// A full fleet snapshot loaded from Postgres.
#[derive(Debug, Clone, Default)]
pub struct FleetSnapshot {
    pub nodes: Vec<FleetNodeRow>,
    pub models: Vec<FleetModelRow>,
}

/// Open a short-lived Postgres pool using the URL from `~/.forgefleet/fleet.toml`.
pub async fn get_fleet_pool() -> Result<PgPool, String> {
    let config_path = dirs::home_dir()
        .ok_or_else(|| "no home dir".to_string())?
        .join(".forgefleet/fleet.toml");
    let toml_str = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("read fleet.toml: {e}"))?;
    let config: ff_core::config::FleetConfig =
        toml::from_str(&toml_str).map_err(|e| format!("parse fleet.toml: {e}"))?;

    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database.url)
        .await
        .map_err(|e| format!("connect Postgres: {e}"))
}

/// Fetch all fleet nodes from Postgres.
pub async fn fetch_nodes() -> Result<Vec<FleetNodeRow>, String> {
    let pool = get_fleet_pool().await?;
    pg_list_nodes(&pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))
}

/// Fetch all fleet models from Postgres.
pub async fn fetch_models() -> Result<Vec<FleetModelRow>, String> {
    let pool = get_fleet_pool().await?;
    pg_list_models(&pool)
        .await
        .map_err(|e| format!("pg_list_models: {e}"))
}

/// Fetch a full snapshot (nodes + models) in a single pool acquisition.
pub async fn fetch_snapshot() -> Result<FleetSnapshot, String> {
    let pool = get_fleet_pool().await?;
    let nodes = pg_list_nodes(&pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    let models = pg_list_models(&pool)
        .await
        .map_err(|e| format!("pg_list_models: {e}"))?;
    Ok(FleetSnapshot { nodes, models })
}

// ─── Secrets ───────────────────────────────────────────────────────────────

/// Fetch a secret by key. Priority: Postgres `fleet_secrets` table, then the
/// corresponding environment variable (e.g. `HF_TOKEN` for `huggingface.token`).
/// Returns `None` if neither source has a value.
pub async fn fetch_secret(key: &str) -> Option<String> {
    if let Ok(pool) = get_fleet_pool().await {
        if let Ok(Some(value)) = pg_get_secret(&pool, key).await {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    // Fallback: environment variable.
    let env_key = env_key_for_secret(key);
    if let Ok(val) = std::env::var(&env_key) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Convenience wrapper for the Hugging Face token.
pub async fn get_hf_token() -> Option<String> {
    fetch_secret("huggingface.token").await
}

/// Map a secret key to its fallback environment variable name.
/// e.g. `huggingface.token` → `HF_TOKEN`, `openai.api_key` → `OPENAI_API_KEY`.
fn env_key_for_secret(key: &str) -> String {
    match key {
        "huggingface.token" => "HF_TOKEN".to_string(),
        "openai.api_key" => "OPENAI_API_KEY".to_string(),
        "anthropic.api_key" => "ANTHROPIC_API_KEY".to_string(),
        other => other.replace('.', "_").to_uppercase(),
    }
}

/// Look up a single node by name (case-insensitive).
pub async fn fetch_node_by_name(name: &str) -> Result<Option<FleetNodeRow>, String> {
    let pool = get_fleet_pool().await?;
    // Try exact match first.
    if let Some(row) = pg_get_node(&pool, name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
    {
        return Ok(Some(row));
    }
    // Case-insensitive fallback.
    let rows = pg_list_nodes(&pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    let lower = name.to_ascii_lowercase();
    Ok(rows.into_iter().find(|r| r.name.to_ascii_lowercase() == lower))
}

/// Look up `(ip, ssh_user)` for a fleet node by name (case-insensitive).
pub async fn fetch_node_ip_user(name: &str) -> Option<(String, String)> {
    match fetch_node_by_name(name).await {
        Ok(Some(row)) => Some((row.ip, row.ssh_user)),
        _ => None,
    }
}

/// Ensure the cached fleet snapshot is populated. Safe to call multiple times.
pub async fn ensure_snapshot_cached() -> &'static FleetSnapshot {
    FLEET_SNAPSHOT
        .get_or_init(|| async {
            match fetch_snapshot().await {
                Ok(snap) => snap,
                Err(err) => {
                    debug!(error = %err, "fleet snapshot unavailable; using empty");
                    FleetSnapshot::default()
                }
            }
        })
        .await
}

/// Read the cached snapshot without initiating a fetch. Returns `None` if
/// the cache hasn't been populated yet.
pub fn cached_snapshot() -> Option<&'static FleetSnapshot> {
    FLEET_SNAPSHOT.get()
}

/// Build a human-readable fleet description from a snapshot. Used in the
/// default agent system prompt so the LLM knows which nodes it can SSH into.
pub fn build_fleet_description(snapshot: &FleetSnapshot) -> String {
    if snapshot.nodes.is_empty() {
        return "No fleet nodes registered in the database. Use NodeEnroll / fleet.toml \
to add nodes."
            .to_string();
    }

    let mut lines = Vec::with_capacity(snapshot.nodes.len());
    for node in &snapshot.nodes {
        let node_models: Vec<&FleetModelRow> = snapshot
            .models
            .iter()
            .filter(|m| m.node_name == node.name)
            .collect();

        let model_desc = if node_models.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = node_models
                .iter()
                .map(|m| format!("{} on port {}", m.name, m.port))
                .collect();
            format!(", running {}", parts.join(" and "))
        };

        lines.push(format!(
            "- {name} ({ip}) — {hardware}, {ram}GB, {os}, role={role}{models}",
            name = node.name,
            ip = node.ip,
            hardware = if node.hardware.is_empty() { "unknown hardware" } else { &node.hardware },
            ram = node.ram_gb,
            os = node.os,
            role = node.role,
            models = model_desc,
        ));
    }

    lines.join("\n")
}

/// Ensure the cached fleet description is populated from Postgres.
/// Returns the cached description.
pub async fn ensure_fleet_description_cached() -> &'static String {
    FLEET_DESCRIPTION
        .get_or_init(|| async {
            match fetch_snapshot().await {
                Ok(snap) => build_fleet_description(&snap),
                Err(err) => {
                    debug!(error = %err, "fleet description unavailable; using placeholder");
                    "No fleet nodes could be loaded from the database. \
Check ~/.forgefleet/fleet.toml [database] url and the fleet_nodes table."
                        .to_string()
                }
            }
        })
        .await
}

/// Return the cached fleet description without initiating a fetch. Returns a
/// generic placeholder if the cache has not been hydrated yet.
pub fn cached_fleet_description() -> String {
    FLEET_DESCRIPTION
        .get()
        .cloned()
        .unwrap_or_else(|| {
            "Fleet topology not yet loaded — call ff_agent::fleet_info::ensure_fleet_description_cached() \
at startup to populate this section."
                .to_string()
        })
}
