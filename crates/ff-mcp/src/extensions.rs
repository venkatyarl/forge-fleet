//! Unified, read-only inventory of ForgeFleet extension capabilities.

use crate::{federation, tools::ToolRegistry};
use serde_json::{Value, json};
use std::path::Path;

/// List skills, MCP tools, and enabled agents through one stable interface.
///
/// Each backing source is best-effort: local tools and on-disk skills remain
/// visible when Postgres or a third-party MCP server is unavailable.
pub async fn list(cwd: &Path, federation_timeout_secs: u64) -> Value {
    let registry = ToolRegistry::new();
    let mut extensions = Vec::new();
    let mut errors = Vec::new();

    for skill in ff_agent::skill_catalog::discover(cwd) {
        extensions.push(json!({
            "kind": "skill", "id": format!("skill/local/{}", skill.id),
            "name": skill.name, "description": skill.description,
            "source": skill.source_root, "path": skill.source_path, "enabled": true,
        }));
    }
    for tool in registry.list() {
        extensions.push(json!({
            "kind": "tool", "id": format!("tool/forgefleet/{}", tool.name),
            "name": tool.name, "description": tool.description,
            "source": "forgefleet", "federated": false, "enabled": true,
        }));
    }

    match ff_core::config::load_config_auto() {
        Ok((config, _)) => {
            let snapshot =
                federation::collect_federation_snapshot(&config, federation_timeout_secs).await;
            for tool in snapshot.tools {
                extensions.push(json!({
                    "kind": "tool", "id": format!("tool/{}/{}", tool.service, tool.name),
                    "name": tool.name, "description": tool.description,
                    "source": tool.service, "endpoint": tool.endpoint,
                    "federated": true, "enabled": true,
                }));
            }
            for service in snapshot.services.into_iter().filter(|s| !s.reachable) {
                errors.push(json!({
                    "source": format!("mcp/{}", service.name),
                    "error": service.error.unwrap_or_else(|| "unreachable".to_string()),
                }));
            }
        }
        Err(error) => errors.push(json!({
            "source": "mcp/federation", "error": error.to_string(),
        })),
    }

    match crate::pool::shared_pg_pool().await {
        Ok(pool) => match ff_db::pg_list_agents(&pool, true).await {
            Ok(agents) => {
                for agent in agents {
                    extensions.push(json!({
                        "kind": "agent", "id": format!("agent/{}/{}", agent.source, agent.name),
                        "name": agent.name, "description": agent.description,
                        "source": agent.source, "role": agent.role, "enabled": agent.enabled,
                    }));
                }
            }
            Err(error) => errors.push(json!({
                "source": "agents/postgres", "error": error.to_string(),
            })),
        },
        Err(error) => errors.push(json!({
            "source": "agents/postgres", "error": error,
        })),
    }

    extensions.sort_by(|a, b| {
        a["kind"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["kind"].as_str().unwrap_or_default())
            .then_with(|| {
                a["id"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(b["id"].as_str().unwrap_or_default())
            })
    });
    let skills = extensions.iter().filter(|e| e["kind"] == "skill").count();
    let tools = extensions.iter().filter(|e| e["kind"] == "tool").count();
    let agents = extensions.iter().filter(|e| e["kind"] == "agent").count();
    json!({
        "count": extensions.len(),
        "counts": { "skills": skills, "tools": tools, "agents": agents },
        "extensions": extensions, "errors": errors,
    })
}
