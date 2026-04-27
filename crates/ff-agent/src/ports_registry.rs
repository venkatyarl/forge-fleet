//! Port registry loader (retired).
//!
//! Historically this module parsed `config/ports.toml` and upserted rows
//! into the `port_registry` Postgres table. That file has been deleted —
//! the DB migration `SCHEMA_V37_RETIRE_PORTS_TOML` now owns the canonical
//! seed set, and operator edits via SQL are preserved across upgrades.
//!
//! The public API ([`seed_from_toml`], [`SeedReport`], and the supporting
//! `PortsFile` / `PortEntry` types) is intentionally preserved so any
//! callers that predate the retirement keep compiling. The seeder itself
//! is now a no-op that logs once and returns an empty [`SeedReport`].
//!
//! Read-side helpers such as [`pick_llm_port`] continue to work against
//! `port_registry` as before.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

/// Errors that can occur while seeding the port registry.
#[derive(Debug, Error)]
pub enum PortsError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Summary returned by [`seed_from_toml`].
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct SeedReport {
    /// Rows that did not previously exist.
    pub inserted: usize,
    /// Rows whose editable fields changed.
    pub updated: usize,
    /// Rows that matched the DB row exactly (no changes).
    pub unchanged: usize,
    /// Total rows processed from the TOML file.
    pub total: usize,
}

/// Top-level TOML document — we only care about `[[port]]` entries.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PortsFile {
    #[serde(default)]
    pub port: Vec<PortEntry>,
}

/// One `[[port]]` entry in the TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PortEntry {
    pub port: i32,
    pub service: String,
    pub kind: String,
    pub description: String,
    pub exposed_on: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub managed_by: Option<String>,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_scope() -> String {
    "lan".to_string()
}

fn default_status() -> String {
    "active".to_string()
}

/// Pick the lowest-numbered free LLM port for `computer_name` + `runtime` by
/// consulting the `port_registry` table.
///
/// Mapping: llama.cpp / mlx → services whose id starts with `llama_cpp_slot_`
/// (55000/55001/55002 in the seeded registry); vllm → services whose id
/// starts with `vllm` (51001/51003); ollama → 11434 (always 11434 since
/// Ollama's routing is internal, so we just return that).
///
/// Excludes ports already bound by active rows in `computer_model_deployments`
/// for the given computer. Returns `sqlx::Error::RowNotFound` when every
/// candidate slot is already taken.
pub async fn pick_llm_port(
    pool: &PgPool,
    computer_name: &str,
    runtime: &str,
) -> Result<i32, sqlx::Error> {
    if runtime == "ollama" {
        return Ok(11434);
    }

    let service_prefix: &str = match runtime {
        "llama.cpp" | "llama_cpp" | "mlx" | "mlx_lm" => "llama_cpp_slot_",
        "vllm" => "vllm",
        _ => return Err(sqlx::Error::RowNotFound),
    };

    // All LLM slots reserved for this runtime family, sorted.
    let candidate_ports: Vec<i32> = sqlx::query_scalar(
        "SELECT port
           FROM port_registry
          WHERE kind = 'llm_inference'
            AND service LIKE $1
          ORDER BY port",
    )
    .bind(format!("{service_prefix}%"))
    .fetch_all(pool)
    .await?;

    // Ports already bound by active deployments on THIS computer.
    // The `endpoint` column is a URL (e.g. http://127.0.0.1:55000) — parse
    // the port out with a regex-friendly substring match at the SQL level
    // so we don't have to pull every row back to Rust.
    let busy_ports: Vec<i32> = sqlx::query_scalar(
        "SELECT (substring(cmd.endpoint from ':(\\d+)(?:/|$)'))::INT AS port
           FROM computer_model_deployments cmd
           JOIN computers c ON c.id = cmd.computer_id
          WHERE LOWER(c.name) = LOWER($1)
            AND cmd.status = 'active'
            AND cmd.endpoint ~ ':\\d+'",
    )
    .bind(computer_name)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    candidate_ports
        .into_iter()
        .find(|p| !busy_ports.contains(p))
        .ok_or(sqlx::Error::RowNotFound)
}

/// Retired no-op seeder. The DB migration `SCHEMA_V37_RETIRE_PORTS_TOML`
/// now owns the canonical `port_registry` seed set; this function is
/// kept only so callers that predate the retirement keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(_pool: &PgPool, _toml_path: &Path) -> Result<SeedReport, PortsError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "port_registry: TOML seeder retired; canonical rows come from migration V37"
        );
    }
    Ok(SeedReport::default())
}

/// Resolve the default `config/ports.toml` path relative to the workspace
/// root, falling back to the current directory if we can't find the repo.
pub fn resolve_ports_path() -> PathBuf {
    // Same resolution strategy as task_coverage_seed + model_catalog_seed.
    for candidate in [
        PathBuf::from("config/ports.toml"),
        PathBuf::from("../config/ports.toml"),
        PathBuf::from("../../config/ports.toml"),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    // Also probe the repo root explicitly — handy when invoked from a
    // launchd / systemd unit with a fixed working directory.
    if let Ok(home) = std::env::var("HOME") {
        let explicit = PathBuf::from(&home).join("projects/forge-fleet/config/ports.toml");
        if explicit.exists() {
            return explicit;
        }
        let worker =
            PathBuf::from(&home).join(".forgefleet/sub-agent-0/forge-fleet/config/ports.toml");
        if worker.exists() {
            return worker;
        }
    }
    PathBuf::from("config/ports.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
schema_version = "1"
updated = "2026-04-19"

[[port]]
port = 51002
service = "forgefleetd"
kind = "control_plane"
description = "ForgeFleet daemon gateway API + web dashboard"
exposed_on = "all_members"
scope = "lan"
managed_by = "launchd/systemd"

[[port]]
port = 55000
service = "llama_cpp_slot_1"
kind = "llm_inference"
description = "llama-server — first model on this computer (primary convention)"
exposed_on = "all_members_with_gguf"
scope = "lan"
managed_by = "manual or ff model load"

[[port]]
port = 26380
service = "redis_sentinel"
kind = "coordination"
description = "Redis Sentinel — DEPRECATED (Pulse v2 replaces this role)"
exposed_on = "taylor"
scope = "lan"
managed_by = "docker compose"
status = "deprecated"

[[range]]
start = 51001
end = 51099
purpose = "test"

[blocklist]
well_known = "0-1023"
"#;

    #[test]
    fn parses_sample_toml_into_expected_shape() {
        let doc: PortsFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        assert_eq!(doc.port.len(), 3);

        let fd = &doc.port[0];
        assert_eq!(fd.port, 51002);
        assert_eq!(fd.service, "forgefleetd");
        assert_eq!(fd.kind, "control_plane");
        assert_eq!(fd.scope, "lan");
        assert_eq!(fd.status, "active");
        assert_eq!(fd.managed_by.as_deref(), Some("launchd/systemd"));

        let depr = &doc.port[2];
        assert_eq!(depr.status, "deprecated");
    }

    #[test]
    fn status_and_scope_defaults_apply() {
        let toml = r#"
[[port]]
port = 1234
service = "x"
kind = "system"
description = "x"
exposed_on = "all_members"
"#;
        let doc: PortsFile = toml::from_str(toml).expect("parse toml");
        assert_eq!(doc.port[0].status, "active");
        assert_eq!(doc.port[0].scope, "lan");
        assert!(doc.port[0].managed_by.is_none());
    }
}
