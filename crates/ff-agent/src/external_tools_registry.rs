//! External-tools registry loader (retired).
//!
//! Historically this module parsed `config/external_tools.toml` and
//! upserted rows into the `external_tools` Postgres table. That file has
//! been deleted — the DB migration `SCHEMA_V38_RETIRE_EXTERNAL_TOOLS_TOML`
//! now owns the canonical seed set, and operator edits via SQL (or a
//! future `ff ext add`) are preserved across upgrades.
//!
//! The public API ([`seed_from_toml`], [`SeedReport`], and the supporting
//! `ExternalToolsFile` / `ExternalToolEntry` types) is intentionally
//! preserved so any callers that predate the retirement keep compiling.
//! The seeder itself is now a no-op that logs once and returns an empty
//! [`SeedReport`]. Read-side helpers ([`list_tools`]) continue to work
//! against Postgres as before.
//!
//! Mirrors [`crate::software_registry`] (V28 retirement template).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use thiserror::Error;

/// Errors that can occur while seeding the external-tools registry.
#[derive(Debug, Error)]
pub enum ExternalToolsError {
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

    #[error("failed to serialize field {field} for {id}: {source}")]
    Json {
        id: String,
        field: &'static str,
        #[source]
        source: serde_json::Error,
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

/// Top-level TOML document: `[[tool]]` array.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExternalToolsFile {
    #[serde(default)]
    pub tool: Vec<ExternalToolEntry>,
}

/// One `[[tool]]` entry in the TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExternalToolEntry {
    pub id: String,
    pub display_name: String,
    pub github_url: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub install_method: String,
    #[serde(default)]
    pub install_spec: toml::value::Table,
    #[serde(default)]
    pub cli_entrypoint: Option<String>,
    #[serde(default)]
    pub mcp_server_command: Option<String>,
    #[serde(default)]
    pub register_as_mcp: bool,
    #[serde(default)]
    pub version_source: toml::value::Table,
    #[serde(default)]
    pub upgrade_playbook: toml::value::Table,
    #[serde(default)]
    pub intake_source: Option<String>,
    #[serde(default)]
    pub intake_reference: Option<String>,
    #[serde(default)]
    pub metadata: toml::value::Table,
}

fn default_kind() -> String {
    "cli".to_string()
}

/// Flat catalog row shape used by [`list_tools`] and CLI printers.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub id: String,
    pub display_name: String,
    pub github_url: String,
    pub kind: String,
    pub install_method: String,
    pub install_spec: serde_json::Value,
    pub cli_entrypoint: Option<String>,
    pub mcp_server_command: Option<String>,
    pub register_as_mcp: bool,
    pub version_source: serde_json::Value,
    pub upgrade_playbook: serde_json::Value,
    pub latest_version: Option<String>,
    pub intake_source: Option<String>,
    pub intake_reference: Option<String>,
}

/// Retired no-op seeder. The DB migration
/// `SCHEMA_V38_RETIRE_EXTERNAL_TOOLS_TOML` now owns the canonical
/// `external_tools` seed set; this function is kept only so callers that
/// predate the retirement keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(
    _pool: &PgPool,
    _toml_path: &Path,
) -> Result<SeedReport, ExternalToolsError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "external_tools: TOML seeder retired; canonical rows come from migration V38"
        );
    }
    Ok(SeedReport::default())
}

/// List every row in `external_tools`, ordered by id.
pub async fn list_tools(pool: &PgPool) -> Result<Vec<Tool>, ExternalToolsError> {
    let rows = sqlx::query(
        "SELECT id,
                display_name,
                github_url,
                kind,
                install_method,
                install_spec,
                cli_entrypoint,
                mcp_server_command,
                register_as_mcp,
                version_source,
                upgrade_playbook,
                latest_version,
                intake_source,
                intake_reference
           FROM external_tools
          ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| Tool {
            id: r.get("id"),
            display_name: r.get("display_name"),
            github_url: r.get("github_url"),
            kind: r.get("kind"),
            install_method: r.get("install_method"),
            install_spec: r.get("install_spec"),
            cli_entrypoint: r.get("cli_entrypoint"),
            mcp_server_command: r.get("mcp_server_command"),
            register_as_mcp: r.get("register_as_mcp"),
            version_source: r.get("version_source"),
            upgrade_playbook: r.get("upgrade_playbook"),
            latest_version: r.get("latest_version"),
            intake_source: r.get("intake_source"),
            intake_reference: r.get("intake_reference"),
        })
        .collect();

    Ok(out)
}

/// Convert a `toml::value::Table` to `serde_json::Value::Object(...)`.
/// Retained for the parser-shape tests below; the runtime seeder is
/// retired (see module docs + V38).
#[allow(dead_code)]
fn toml_table_to_json(
    table: &toml::value::Table,
) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
[[tool]]
id = "code-review-graph"
display_name = "Code Review Graph"
github_url = "https://github.com/anthropics/code-review-graph"
kind = "mcp"
install_method = "cargo_install"
install_spec = { repo = "anthropics/code-review-graph", bin = "code-review-graph-mcp" }
cli_entrypoint = "crg"
mcp_server_command = "code-review-graph-mcp --stdio"
register_as_mcp = true
version_source = { method = "github_release", repo = "anthropics/code-review-graph" }

[tool.upgrade_playbook]
all = "cargo install --git https://github.com/anthropics/code-review-graph --force"

[[tool]]
id = "context-mode"
display_name = "Context Mode"
github_url = "https://github.com/context-mode/context-mode"
kind = "mcp"
install_method = "npm_global"
install_spec = { package = "@context-mode/mcp" }
cli_entrypoint = "context-mode"
register_as_mcp = true
version_source = { method = "github_release", repo = "context-mode/context-mode" }

[tool.upgrade_playbook]
all = "npm install -g @context-mode/mcp@latest"
"#;

    #[test]
    fn parses_sample_toml_into_expected_shape() {
        let doc: ExternalToolsFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        assert_eq!(doc.tool.len(), 2);

        let crg = &doc.tool[0];
        assert_eq!(crg.id, "code-review-graph");
        assert_eq!(crg.kind, "mcp");
        assert_eq!(crg.install_method, "cargo_install");
        assert_eq!(crg.cli_entrypoint.as_deref(), Some("crg"));
        assert!(crg.register_as_mcp);
        assert_eq!(
            crg.install_spec.get("repo").and_then(|v| v.as_str()),
            Some("anthropics/code-review-graph")
        );
        assert_eq!(
            crg.upgrade_playbook.get("all").and_then(|v| v.as_str()),
            Some("cargo install --git https://github.com/anthropics/code-review-graph --force")
        );

        let ctx = &doc.tool[1];
        assert_eq!(ctx.id, "context-mode");
        assert_eq!(ctx.install_method, "npm_global");
        assert!(ctx.mcp_server_command.is_none());
    }

    #[test]
    fn install_spec_round_trips_to_json() {
        let doc: ExternalToolsFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        let crg = &doc.tool[0];
        let js = toml_table_to_json(&crg.install_spec).expect("install_spec to json");
        assert_eq!(
            js.get("repo").and_then(|v| v.as_str()),
            Some("anthropics/code-review-graph")
        );
        assert_eq!(
            js.get("bin").and_then(|v| v.as_str()),
            Some("code-review-graph-mcp")
        );
    }
}
