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

/// Process-wide Postgres pool shared by every `fleet_info::*` helper.
///
/// Prior to 2026-04-23 this helper built a fresh `PgPool` on every call.
/// Callers drop the pool after one query, but sqlx closes connections in
/// a background task, so high-frequency callers (disk_sampler,
/// deployment_reconciler, model_runtime, version_check, orchestrator,
/// every LLM tool, every heartbeat tick) accumulated dozens of idle
/// backends on Postgres and burned an ephemeral port per connection on
/// the client — tripped `too many clients already` at 100-slot cap and
/// `EADDRNOTAVAIL` on macOS once 16K source ports were TIME_WAIT.
static FLEET_POOL: OnceCell<PgPool> = OnceCell::const_new();

/// A full fleet snapshot loaded from Postgres.
#[derive(Debug, Clone, Default)]
pub struct FleetSnapshot {
    pub nodes: Vec<FleetNodeRow>,
    pub models: Vec<FleetModelRow>,
}

/// Return the process-wide shared Postgres pool, initializing it on first
/// call from `~/.forgefleet/fleet.toml`.
pub async fn get_fleet_pool() -> Result<PgPool, String> {
    if let Some(p) = FLEET_POOL.get() {
        return Ok(p.clone());
    }
    let pool = build_fleet_pool().await?;
    Ok(FLEET_POOL.get_or_init(|| async { pool }).await.clone())
}

async fn build_fleet_pool() -> Result<PgPool, String> {
    let config_path = dirs::home_dir()
        .ok_or_else(|| "no home dir".to_string())?
        .join(".forgefleet/fleet.toml");
    let toml_str = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("read fleet.toml: {e}"))?;
    let config: ff_core::config::FleetConfig =
        toml::from_str(&toml_str).map_err(|e| format!("parse fleet.toml: {e}"))?;

    PgPoolOptions::new()
        .max_connections(10)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Some(Duration::from_secs(60)))
        .max_lifetime(Some(Duration::from_secs(30 * 60)))
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

/// Resolve the ForgeFleet node name for the CURRENT host, in priority order:
///   1. `$FORGEFLEET_NODE_NAME` env var (explicit override)
///   2. Postgres `fleet_nodes` row whose `ip` or `alt_ips` matches any local IPv4 address
///   3. `hostname` short-name fallback (lowercased, first label)
pub async fn resolve_this_node_name() -> String {
    if let Ok(v) = std::env::var("FORGEFLEET_NODE_NAME") {
        let t = v.trim();
        if !t.is_empty() { return t.to_string(); }
    }

    // Collect local IPv4 addresses.
    let local_ips = local_ipv4_addrs();

    if let Ok(pool) = get_fleet_pool().await {
        if let Ok(nodes) = pg_list_nodes(&pool).await {
            for n in &nodes {
                if local_ips.contains(&n.ip) {
                    return n.name.clone();
                }
                // alt_ips is JSONB array of strings
                if let Some(alt) = n.alt_ips.as_array() {
                    for v in alt {
                        if let Some(s) = v.as_str() {
                            if local_ips.contains(&s.to_string()) {
                                return n.name.clone();
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: hostname short name.
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().split('.').next().unwrap_or("unknown").to_lowercase())
        .unwrap_or_else(|| "unknown".into())
}

/// Enumerate local IPv4 addresses (non-loopback) via `ifconfig -l`.
/// Returns an empty vec on any error; callers then fall through to hostname.
fn local_ipv4_addrs() -> Vec<String> {
    // Use `ifconfig` with `-a` (BSD/mac: lists all interfaces with addresses).
    let out = std::process::Command::new("ifconfig").arg("-a").output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ips = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("inet ") {
            // rest looks like "192.168.5.100 netmask ..."
            if let Some(addr) = rest.split_whitespace().next() {
                if !addr.starts_with("127.") && !addr.starts_with("169.254") {
                    ips.push(addr.to_string());
                }
            }
        }
    }
    ips
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

/// Resolve the best reachable IP for a computer by name, in priority order:
///   1. Any entry in `computers.all_ips` with `kind == "lan"`.
///   2. `computers.primary_ip` if it looks LAN-routable
///      (i.e. 10.x, 192.168.x, or 172.16-31.x).
///   3. Any entry in `computers.all_ips` with `kind == "tailscale"`.
///   4. `computers.primary_ip` as a last resort (may be a Tailscale IP
///      already on a Tailscale-only box).
///
/// Returns `(ip, kind)` where `kind` is one of `"lan"`, `"tailscale"`,
/// `"wireguard"`, or `"public"`. Returns `None` if no computer row exists.
///
/// Call sites should use this when opening an SSH/probe connection to a peer
/// — it prefers LAN for latency/cost but transparently falls back to an
/// overlay network when that's all the box has.
pub async fn resolve_best_ip(computer_name: &str) -> Option<(String, String)> {
    let pool = get_fleet_pool().await.ok()?;
    let row = sqlx::query_as::<_, (String, serde_json::Value, String)>(
        "SELECT primary_ip, all_ips, network_scope
         FROM computers
         WHERE LOWER(name) = LOWER($1)",
    )
    .bind(computer_name)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten()?;

    let (primary_ip, all_ips, network_scope) = row;

    // Parse all_ips into (ip, kind) pairs.
    let pairs: Vec<(String, String)> = all_ips
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let obj = v.as_object()?;
                    let ip = obj.get("ip")?.as_str()?.to_string();
                    let kind = obj
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    Some((ip, kind))
                })
                .collect()
        })
        .unwrap_or_default();

    // If explicitly tailscale_only, prefer the tailscale entry straight away.
    if network_scope == "tailscale_only" {
        if let Some((ip, kind)) = pairs.iter().find(|(_, k)| k == "tailscale") {
            return Some((ip.clone(), kind.clone()));
        }
    }

    // 1. LAN from all_ips.
    if let Some((ip, kind)) = pairs.iter().find(|(_, k)| k == "lan") {
        return Some((ip.clone(), kind.clone()));
    }

    // 2. primary_ip if it looks LAN-routable.
    if is_lan_ip(&primary_ip) {
        return Some((primary_ip, "lan".to_string()));
    }

    // 3. Tailscale from all_ips.
    if let Some((ip, kind)) = pairs.iter().find(|(_, k)| k == "tailscale") {
        return Some((ip.clone(), kind.clone()));
    }

    // 4. primary_ip as last resort — classify it so the caller can decide.
    let kind = classify_ip(&primary_ip);
    Some((primary_ip, kind))
}

fn is_lan_ip(ip: &str) -> bool {
    ip.starts_with("10.")
        || ip.starts_with("192.168.")
        || ip.starts_with("172.16.")
        || ip.starts_with("172.17.")
        || ip.starts_with("172.18.")
        || ip.starts_with("172.19.")
        || ip.starts_with("172.2")
        || ip.starts_with("172.30.")
        || ip.starts_with("172.31.")
}

fn classify_ip(ip: &str) -> String {
    if ip.starts_with("100.64.") || ip.starts_with("100.65.") {
        "tailscale".to_string()
    } else if is_lan_ip(ip) {
        "lan".to_string()
    } else {
        "public".to_string()
    }
}

/// Fetch a computer's `network_scope` setting. Returns "lan" as the safe
/// default if the row does not exist or the column is unset.
pub async fn fetch_network_scope(computer_name: &str) -> String {
    let Ok(pool) = get_fleet_pool().await else { return "lan".to_string() };
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT network_scope FROM computers WHERE LOWER(name) = LOWER($1)",
    )
    .bind(computer_name)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    row.map(|r| r.0).unwrap_or_else(|| "lan".to_string())
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
