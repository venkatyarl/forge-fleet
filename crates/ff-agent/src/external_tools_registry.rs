//! External-tools registry loader.
//!
//! Parses `config/external_tools.toml` into rows and upserts them into
//! the `external_tools` Postgres table (schema V24).
//!
//! The table has `latest_version` + `latest_version_at` columns which
//! are owned by the upstream-check loop — this loader NEVER writes those
//! two columns, only INSERTs them as NULL on first insert.
//!
//! Mirrors [`crate::software_registry`]. If you change the schema here,
//! check that module too so they don't drift apart.

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

/// Read the external-tools TOML from `path` and upsert every row into
/// `external_tools`. Returns a per-row summary.
///
/// The SQL uses `INSERT ... ON CONFLICT (id) DO UPDATE SET ...` and updates
/// every column EXCEPT `latest_version` and `latest_version_at`, which are
/// owned by the upstream-check loop.
pub async fn seed_from_toml(
    pool: &PgPool,
    toml_path: &Path,
) -> Result<SeedReport, ExternalToolsError> {
    let raw = std::fs::read_to_string(toml_path).map_err(|source| ExternalToolsError::Io {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let doc: ExternalToolsFile =
        toml::from_str(&raw).map_err(|source| ExternalToolsError::Toml {
            path: toml_path.to_path_buf(),
            source,
        })?;

    let mut report = SeedReport {
        total: doc.tool.len(),
        ..SeedReport::default()
    };

    for entry in &doc.tool {
        let install_spec =
            toml_table_to_json(&entry.install_spec).map_err(|source| ExternalToolsError::Json {
                id: entry.id.clone(),
                field: "install_spec",
                source,
            })?;

        let version_source =
            toml_table_to_json(&entry.version_source).map_err(|source| {
                ExternalToolsError::Json {
                    id: entry.id.clone(),
                    field: "version_source",
                    source,
                }
            })?;

        let upgrade_playbook = toml_table_to_json(&entry.upgrade_playbook).map_err(|source| {
            ExternalToolsError::Json {
                id: entry.id.clone(),
                field: "upgrade_playbook",
                source,
            }
        })?;

        let metadata =
            toml_table_to_json(&entry.metadata).map_err(|source| ExternalToolsError::Json {
                id: entry.id.clone(),
                field: "metadata",
                source,
            })?;

        // xmax=0 trick identifies freshly-inserted rows; changed-detection
        // compares each editable column against the pre-image.
        let row: Option<(bool, bool)> = sqlx::query_as(
            r#"
            WITH existing AS (
                SELECT
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
                    intake_source,
                    intake_reference,
                    metadata
                FROM external_tools
                WHERE id = $1
            ),
            upsert AS (
                INSERT INTO external_tools (
                    id,
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
                    intake_source,
                    intake_reference,
                    metadata
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                ON CONFLICT (id) DO UPDATE SET
                    display_name       = EXCLUDED.display_name,
                    github_url         = EXCLUDED.github_url,
                    kind               = EXCLUDED.kind,
                    install_method     = EXCLUDED.install_method,
                    install_spec       = EXCLUDED.install_spec,
                    cli_entrypoint     = EXCLUDED.cli_entrypoint,
                    mcp_server_command = EXCLUDED.mcp_server_command,
                    register_as_mcp    = EXCLUDED.register_as_mcp,
                    version_source     = EXCLUDED.version_source,
                    upgrade_playbook   = EXCLUDED.upgrade_playbook,
                    intake_source      = EXCLUDED.intake_source,
                    intake_reference   = EXCLUDED.intake_reference,
                    metadata           = EXCLUDED.metadata
                RETURNING (xmax = 0) AS inserted
            )
            SELECT
                u.inserted,
                COALESCE(
                    e.display_name       IS DISTINCT FROM $2  OR
                    e.github_url         IS DISTINCT FROM $3  OR
                    e.kind               IS DISTINCT FROM $4  OR
                    e.install_method     IS DISTINCT FROM $5  OR
                    e.install_spec       IS DISTINCT FROM $6  OR
                    e.cli_entrypoint     IS DISTINCT FROM $7  OR
                    e.mcp_server_command IS DISTINCT FROM $8  OR
                    e.register_as_mcp    IS DISTINCT FROM $9  OR
                    e.version_source     IS DISTINCT FROM $10 OR
                    e.upgrade_playbook   IS DISTINCT FROM $11 OR
                    e.intake_source      IS DISTINCT FROM $12 OR
                    e.intake_reference   IS DISTINCT FROM $13 OR
                    e.metadata           IS DISTINCT FROM $14,
                    true
                ) AS changed
            FROM upsert u
            LEFT JOIN existing e ON TRUE
            "#,
        )
        .bind(&entry.id)
        .bind(&entry.display_name)
        .bind(&entry.github_url)
        .bind(&entry.kind)
        .bind(&entry.install_method)
        .bind(&install_spec)
        .bind(entry.cli_entrypoint.as_deref())
        .bind(entry.mcp_server_command.as_deref())
        .bind(entry.register_as_mcp)
        .bind(&version_source)
        .bind(&upgrade_playbook)
        .bind(entry.intake_source.as_deref())
        .bind(entry.intake_reference.as_deref())
        .bind(&metadata)
        .fetch_optional(pool)
        .await?;

        match row {
            Some((true, _)) => report.inserted += 1,
            Some((false, true)) => report.updated += 1,
            Some((false, false)) => report.unchanged += 1,
            None => report.updated += 1,
        }
    }

    Ok(report)
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
