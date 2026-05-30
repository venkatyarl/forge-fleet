//! Fleet agents (V112) — DB row helpers + runtime catalog renderer +
//! optional on-disk `AGENT.md` importer.
//!
//! This is the AGENTS analogue of [`crate::skills_db`] (V105). The Postgres
//! `fleet_agents` table is the canonical source of truth for every specialized
//! agent ForgeFleet can instantiate (code-writer, code-reviewer, researcher,
//! …). Until V112 there were THREE disconnected role representations
//! (fleet_crew's hardcoded pipeline, `ff_orchestrator::crew::AgentRole`, and
//! `ff_agent::agent_roles`) and no catalog — this module + the table unify
//! them behind one DB-backed list.
//!
//! Each row maps a stable `name` (e.g. "code-writer") → a system_prompt +
//! allowed_tools + a routing capability (`require_tool_calling` + `min_ctx`).
//! The crew / orchestrator reads members from here by name and routes each one
//! through the V111 agent-swarm capability router (`pg_pick_agent_endpoint`)
//! rather than hardcoding Taylor.
//!
//! Two parallel surfaces use this data (mirroring `skills_db`):
//!   - **CLI** (`ff agents list / show`) reads the DB directly.
//!   - **Runtime catalog** ([`render_catalog`]) renders a compact block an
//!     orchestrator can inject into a planning prompt so a coordinating agent
//!     can self-route to the right specialist — the same idea as
//!     `skill_catalog::render_catalog`.
//!
//! The importer ([`import_repo_agents`]) walks `AGENT.md` files (YAML
//! frontmatter + markdown body) so a repo of agent definitions can be loaded
//! the same way `ff skills import` loads `SKILL.md` files.

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use std::path::{Path, PathBuf};

pub use ff_db::FleetAgentRow;

/// Default per-slot ctx floor for an instantiated agent when a row leaves
/// `min_ctx` at its column default. Matches the V112 column default.
pub const DEFAULT_AGENT_MIN_CTX: i32 = 16384;

/// List agents from the catalog. `enabled_only = true` returns only enabled
/// rows (the set the crew/router should consider).
pub async fn list_all(pool: &PgPool, enabled_only: bool) -> Result<Vec<FleetAgentRow>> {
    ff_db::pg_list_agents(pool, enabled_only)
        .await
        .context("list fleet_agents")
}

/// Fetch one agent by its stable `name` handle.
pub async fn get_by_name(pool: &PgPool, name: &str) -> Result<Option<FleetAgentRow>> {
    ff_db::pg_get_agent(pool, name)
        .await
        .context("get fleet_agent by name")
}

/// Helper: pull the `allowed_tools` jsonb array off a row as a `Vec<String>`.
/// Empty means "inherit the session default tool set".
pub fn allowed_tools(row: &FleetAgentRow) -> Vec<String> {
    json_str_array(&row.allowed_tools)
}

/// Helper: pull the `triggers` jsonb array off a row as a `Vec<String>`.
pub fn triggers(row: &FleetAgentRow) -> Vec<String> {
    json_str_array(&row.triggers)
}

fn json_str_array(v: &JsonValue) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Render the catalog block an orchestrator can prepend to a planning prompt
/// so a coordinating agent can self-route to the right specialist. Empty input
/// returns an empty string (no block injected). Mirrors
/// `skill_catalog::render_catalog`.
pub fn render_catalog(agents: &[FleetAgentRow]) -> String {
    if agents.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Agents available in the fleet catalog\n\n\
         Each agent is a specialist with its own system prompt, tool set, and \
         routing capability. When delegating a subtask, pick the agent whose \
         role + triggers best match the work.\n\n",
    );
    for a in agents {
        let trig = triggers(a);
        let trig_str = if trig.is_empty() {
            String::new()
        } else {
            format!(
                " · triggers: {}",
                trig.iter()
                    .take(8)
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!(
            "- **`{name}`** — {role}{trig}\n  {desc}\n",
            name = a.name,
            role = a.role,
            trig = trig_str,
            desc = a.description.as_deref().unwrap_or(""),
        ));
    }
    out.push_str("\n---\n\n");
    out
}

// ─── Importer (optional on-disk AGENT.md source) ─────────────────────────────

/// Import every `AGENT.md` under `repo_dir` into the catalog. Mirrors
/// `skills_db::import_repo_skills`. `source` is recorded on each row. Returns
/// (imported_or_updated, errors).
pub async fn import_repo_agents(
    pool: &PgPool,
    repo_dir: &Path,
    source: &str,
    source_url: Option<&str>,
) -> Result<(usize, usize)> {
    let mut imported = 0;
    let mut errors = 0;
    for path in find_agent_files(repo_dir)? {
        match parse_agent_file(&path) {
            Ok(a) => {
                let tools = JsonValue::Array(
                    a.allowed_tools
                        .iter()
                        .map(|s| JsonValue::String(s.clone()))
                        .collect(),
                );
                let trig = JsonValue::Array(
                    a.triggers
                        .iter()
                        .map(|s| JsonValue::String(s.clone()))
                        .collect(),
                );
                let role = a.role.clone().unwrap_or_else(|| a.name.clone());
                match ff_db::pg_upsert_agent(
                    pool,
                    &a.name,
                    &role,
                    a.description.as_deref(),
                    &a.system_prompt,
                    &tools,
                    &trig,
                    a.require_tool_calling.unwrap_or(true),
                    a.min_ctx.unwrap_or(DEFAULT_AGENT_MIN_CTX),
                    source,
                    source_url,
                )
                .await
                {
                    Ok(_) => imported += 1,
                    Err(e) => {
                        eprintln!("warn: upsert agent {} failed: {e}", a.name);
                        errors += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("warn: read {} failed: {e}", path.display());
                errors += 1;
            }
        }
    }
    Ok((imported, errors))
}

fn find_agent_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(f) => f,
                Err(_) => continue,
            };
            let path = entry.path();
            if ft.is_dir() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s == ".git" || s == "node_modules" || s == "target" {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.eq_ignore_ascii_case("AGENT.md"))
                    .unwrap_or(false)
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[derive(Debug, Default)]
struct ParsedAgent {
    name: String,
    role: Option<String>,
    description: Option<String>,
    system_prompt: String,
    allowed_tools: Vec<String>,
    triggers: Vec<String>,
    require_tool_calling: Option<bool>,
    min_ctx: Option<i32>,
}

/// Parse an `AGENT.md` — YAML frontmatter (name/role/description/triggers/
/// tools/require_tool_calling/min_ctx) + a markdown body used as the
/// system_prompt. Falls back to the directory name when `name` is absent.
fn parse_agent_file(path: &Path) -> Result<ParsedAgent> {
    let raw = std::fs::read_to_string(path)?;
    let trimmed = raw.trim_start_matches('\u{feff}');
    let mut a = ParsedAgent::default();
    let body: String;
    if let Some(rest) = trimmed.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        parse_frontmatter(&rest[..end], &mut a);
        body = rest[end + 5..].trim().to_string();
    } else {
        body = raw.trim().to_string();
    }
    if a.name.is_empty() {
        a.name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
    }
    // The body is the system prompt; if the frontmatter carried a description
    // but the body is empty, fall back to the description.
    a.system_prompt = if body.is_empty() {
        a.description.clone().unwrap_or_default()
    } else {
        body
    };
    Ok(a)
}

/// Cheap line-oriented frontmatter parser for the subset of keys we expect —
/// avoids pulling a YAML dep, same approach as `skills_db::parse_frontmatter_loose`.
fn parse_frontmatter(text: &str, a: &mut ParsedAgent) {
    let mut iter = text.lines().peekable();
    while let Some(line) = iter.next() {
        let line = line.trim_end();
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_lowercase();
        let raw_v = v.trim_start();
        // List form: `tools:` / `triggers:` followed by `- item`
        if raw_v.is_empty() && matches!(key.as_str(), "tools" | "allowed_tools" | "triggers") {
            let mut items = Vec::new();
            while let Some(next) = iter.peek() {
                let t = next.trim_start();
                if let Some(item) = t.strip_prefix("- ") {
                    items.push(item.trim().trim_matches('"').to_string());
                    iter.next();
                } else if next.trim().is_empty() {
                    iter.next();
                } else {
                    break;
                }
            }
            if key == "triggers" {
                a.triggers = items;
            } else {
                a.allowed_tools = items;
            }
            continue;
        }
        let value = raw_v.trim_matches('"').trim_matches('\'').to_string();
        match key.as_str() {
            "name" => a.name = value,
            "role" => a.role = Some(value),
            "description" => a.description = Some(value),
            "require_tool_calling" | "tool_calling" => {
                a.require_tool_calling = Some(value.eq_ignore_ascii_case("true"))
            }
            "min_ctx" => a.min_ctx = value.parse().ok(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_catalog_empty_is_empty() {
        assert_eq!(render_catalog(&[]), "");
    }

    #[test]
    fn render_catalog_includes_name_role_triggers() {
        let row = FleetAgentRow {
            id: "00000000-0000-0000-0000-000000000000".into(),
            name: "code-writer".into(),
            role: "Code Writer".into(),
            description: Some("Implements changes.".into()),
            system_prompt: "be a coder".into(),
            allowed_tools: serde_json::json!(["Read", "Edit"]),
            triggers: serde_json::json!(["write code", "fix bug"]),
            require_tool_calling: true,
            min_ctx: 16384,
            source: "forgefleet".into(),
            source_url: None,
            enabled: true,
        };
        let r = render_catalog(std::slice::from_ref(&row));
        assert!(r.contains("`code-writer`"));
        assert!(r.contains("Code Writer"));
        assert!(r.contains("Implements changes."));
        assert!(r.contains("`write code`, `fix bug`"));
        assert_eq!(allowed_tools(&row), vec!["Read", "Edit"]);
    }

    #[test]
    fn parse_agent_frontmatter_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let md = "---\nname: tester\nrole: Test Writer\ndescription: writes tests\nmin_ctx: 32768\nrequire_tool_calling: true\ntools:\n  - Read\n  - Bash\ntriggers:\n  - write tests\n---\n\nYou are a testing specialist.\n";
        std::fs::write(agent_dir.join("AGENT.md"), md).unwrap();
        let files = find_agent_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        let a = parse_agent_file(&files[0]).unwrap();
        assert_eq!(a.name, "tester");
        assert_eq!(a.role.as_deref(), Some("Test Writer"));
        assert_eq!(a.min_ctx, Some(32768));
        assert_eq!(a.require_tool_calling, Some(true));
        assert_eq!(a.allowed_tools, vec!["Read", "Bash"]);
        assert_eq!(a.triggers, vec!["write tests"]);
        assert_eq!(a.system_prompt, "You are a testing specialist.");
    }

    #[test]
    fn parse_agent_falls_back_to_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("explorer");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("AGENT.md"), "Just a body, no frontmatter.").unwrap();
        let a = parse_agent_file(&agent_dir.join("AGENT.md")).unwrap();
        assert_eq!(a.name, "explorer");
        assert_eq!(a.system_prompt, "Just a body, no frontmatter.");
    }
}
