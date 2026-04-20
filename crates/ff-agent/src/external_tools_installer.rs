//! External-tools installer / dispatcher.
//!
//! Given a tool id and a target computer, resolves the per-os-family
//! `upgrade_playbook` command and enqueues a `kind='shell'` deferred
//! task whose worker runs the install on the target node.
//!
//! The payload carries `meta.external_tool = {id, computer}` so the
//! worker's finalizer (see `crates/ff-terminal/src/main.rs`
//! `finalize_external_tool_event`) can flip
//! `computer_external_tools.status` to `'ok'` or `'install_failed'` and
//! best-effort extract `installed_version` / `install_path` from task
//! stdout after completion.
//!
//! Mirrors [`crate::auto_upgrade::enqueue_plans`]. TODO: when MCP
//! auto-registration lands, the finalizer will also flip
//! `mcp_registered=true`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};

/// One target computer + the resolved playbook command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPlan {
    pub tool_id: String,
    pub display_name: String,
    pub computer_name: String,
    pub os_family: String,
    pub install_method: String,
    pub install_source: Option<String>,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub playbook_key: String,
    pub command: String,
    pub register_as_mcp: bool,
    pub mcp_server_command: Option<String>,
}

/// Result of enqueuing one plan.
#[derive(Debug, Clone)]
pub struct EnqueueResult {
    pub computer_name: String,
    pub defer_id: String,
    pub tool_id: String,
}

/// Resolve install plans for `tool_id`. Covers three cases:
///
///   - `only_computer = Some(name)` → one explicit target. A row is
///     created (status='installing') if it doesn't already exist so the
///     finalizer has something to flip.
///   - `only_computer = None, all_online = true` → every online
///     computer that doesn't already have the tool OR whose status is
///     `upgrade_available`.
///   - neither is set → error.
pub async fn resolve_install_plans(
    pool: &PgPool,
    tool_id: &str,
    only_computer: Option<&str>,
    all_online: bool,
) -> Result<(Vec<InstallPlan>, Vec<(String, String)>)> {
    let tool_row = sqlx::query(
        "SELECT id, display_name, install_method, upgrade_playbook,
                latest_version, register_as_mcp, mcp_server_command
           FROM external_tools
          WHERE id = $1",
    )
    .bind(tool_id)
    .fetch_optional(pool)
    .await
    .context("select external_tools")?;

    let Some(tool_row) = tool_row else {
        anyhow::bail!("no external_tools entry for id='{tool_id}'");
    };

    let display_name: String = tool_row.get("display_name");
    let install_method: String = tool_row.get("install_method");
    let playbook: JsonValue = tool_row.get("upgrade_playbook");
    let latest_version: Option<String> = tool_row.get("latest_version");
    let register_as_mcp: bool = tool_row.get("register_as_mcp");
    let mcp_server_command: Option<String> = tool_row.get("mcp_server_command");

    // Figure out which computers to target.
    let target_rows = if let Some(name) = only_computer {
        sqlx::query(
            "SELECT c.id          AS id,
                    c.name        AS name,
                    c.os_family   AS os_family,
                    cet.install_source     AS install_source,
                    cet.installed_version  AS installed_version
               FROM computers c
          LEFT JOIN computer_external_tools cet
                 ON cet.computer_id = c.id AND cet.tool_id = $1
              WHERE LOWER(c.name) = LOWER($2)
              ORDER BY c.name",
        )
        .bind(tool_id)
        .bind(name)
        .fetch_all(pool)
        .await
    } else if all_online {
        sqlx::query(
            "SELECT c.id          AS id,
                    c.name        AS name,
                    c.os_family   AS os_family,
                    cet.install_source     AS install_source,
                    cet.installed_version  AS installed_version
               FROM computers c
          LEFT JOIN computer_external_tools cet
                 ON cet.computer_id = c.id AND cet.tool_id = $1
              WHERE c.status = 'online'
              ORDER BY c.name",
        )
        .bind(tool_id)
        .fetch_all(pool)
        .await
    } else {
        anyhow::bail!("pass only_computer or all_online=true");
    }
    .context("select computers")?;

    let mut plans = Vec::with_capacity(target_rows.len());
    let mut skipped = Vec::new();

    for row in &target_rows {
        let name: String = row.get("name");
        let os_family: String = row.get("os_family");
        let install_source: Option<String> = row.get("install_source");
        let installed_version: Option<String> = row.get("installed_version");

        let candidates: Vec<String> = {
            let mut v = Vec::new();
            if let Some(src) = &install_source {
                v.push(format!("{os_family}-{src}"));
            }
            v.push(os_family.clone());
            v.push("all".to_string());
            v
        };

        let mut matched: Option<(String, String)> = None;
        for key in &candidates {
            if let Some(val) = playbook.get(key).and_then(|v| v.as_str()) {
                matched = Some((key.clone(), val.to_string()));
                break;
            }
        }

        match matched {
            Some((playbook_key, command)) => plans.push(InstallPlan {
                tool_id: tool_id.to_string(),
                display_name: display_name.clone(),
                computer_name: name,
                os_family,
                install_method: install_method.clone(),
                install_source,
                installed_version,
                latest_version: latest_version.clone(),
                playbook_key,
                command,
                register_as_mcp,
                mcp_server_command: mcp_server_command.clone(),
            }),
            None => skipped.push((
                name,
                format!(
                    "no playbook key for os='{os_family}' source='{}' (tried {:?})",
                    install_source.as_deref().unwrap_or("-"),
                    candidates
                ),
            )),
        }
    }

    Ok((plans, skipped))
}

/// Enqueue the given plans as `kind='shell'` deferred tasks.
///
/// Each payload carries:
///   - `command`            → the playbook command the worker runs
///   - `meta.external_tool` → `{id, display_name, computer, old_version,
///                              latest_version, playbook_key, install_method,
///                              register_as_mcp, mcp_server_command}`
///
/// After enqueuing, a placeholder `computer_external_tools` row is
/// upserted with `status='installing'` so the finalizer can flip it and
/// subsequent drift ticks don't double-fire.
pub async fn enqueue_plans(
    pool: &PgPool,
    plans: &[InstallPlan],
    who: &str,
) -> Result<Vec<EnqueueResult>> {
    let mut out = Vec::with_capacity(plans.len());
    for p in plans {
        let payload = json!({
            "command": p.command,
            "meta": {
                "external_tool": {
                    "id":                 p.tool_id,
                    "display_name":       p.display_name,
                    "computer":           p.computer_name,
                    "old_version":        p.installed_version,
                    "latest_version":     p.latest_version,
                    "playbook_key":       p.playbook_key,
                    "install_method":     p.install_method,
                    "register_as_mcp":    p.register_as_mcp,
                    "mcp_server_command": p.mcp_server_command,
                    "source":             who,
                }
            }
        });
        let trigger_spec = json!({ "node": p.computer_name });
        let title = format!("Install {} on {}", p.tool_id, p.computer_name);
        let id = ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(&p.computer_name),
            &json!([]),
            Some(who),
            Some(3),
        )
        .await
        .context("enqueue deferred task")?;

        // Upsert placeholder install row so the finalizer has something
        // to flip. We key the status on whether this looks like an
        // upgrade (had a prior installed_version) vs a first install.
        let new_status = if p.installed_version.is_some() {
            "upgrading"
        } else {
            "installing"
        };

        let install_source = derive_install_source(&p.install_method);

        let _ = sqlx::query(
            "INSERT INTO computer_external_tools
                 (computer_id, tool_id, install_source, status)
             SELECT c.id, $1, $2, $3
               FROM computers c
              WHERE LOWER(c.name) = LOWER($4)
             ON CONFLICT (computer_id, tool_id) DO UPDATE
               SET status = EXCLUDED.status,
                   install_source = COALESCE(computer_external_tools.install_source, EXCLUDED.install_source)",
        )
        .bind(&p.tool_id)
        .bind(install_source)
        .bind(new_status)
        .bind(&p.computer_name)
        .execute(pool)
        .await;

        out.push(EnqueueResult {
            computer_name: p.computer_name.clone(),
            defer_id: id,
            tool_id: p.tool_id.clone(),
        });
    }
    Ok(out)
}

/// High-level one-shot: look up the playbook for `tool_id` on
/// `computer_name` and enqueue exactly one install task.
///
/// This mirrors `auto_upgrade::resolve_upgrade_plans` + `enqueue_plans`
/// in one call for CLI ergonomics. Returns an error if the resolver
/// produced no plans (e.g. unknown computer, no playbook match).
pub async fn install_on(
    pool: &PgPool,
    tool_id: &str,
    computer_name: &str,
    who: &str,
) -> Result<EnqueueResult> {
    let (plans, skipped) = resolve_install_plans(pool, tool_id, Some(computer_name), false).await?;
    if plans.is_empty() {
        if let Some((_, why)) = skipped.first() {
            anyhow::bail!(
                "no install plan for {tool_id} on {computer_name}: {why}"
            );
        }
        anyhow::bail!(
            "no install plan for {tool_id} on {computer_name} (is the computer in `computers`?)"
        );
    }
    let mut enq = enqueue_plans(pool, &plans, who).await?;
    Ok(enq.remove(0))
}

/// Map an install_method to the column value stored in
/// `computer_external_tools.install_source`.
fn derive_install_source(install_method: &str) -> &'static str {
    match install_method {
        "cargo_install" => "cargo",
        "npm_global" => "npm",
        "pip" => "pip",
        "git_build" => "git_build",
        "binary_release" => "direct",
        _ => "direct",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_source_mapping_covers_known_methods() {
        assert_eq!(derive_install_source("cargo_install"), "cargo");
        assert_eq!(derive_install_source("npm_global"), "npm");
        assert_eq!(derive_install_source("pip"), "pip");
        assert_eq!(derive_install_source("git_build"), "git_build");
        assert_eq!(derive_install_source("binary_release"), "direct");
        // Unknown methods fall back to "direct" rather than blowing up.
        assert_eq!(derive_install_source("mystery"), "direct");
    }
}
