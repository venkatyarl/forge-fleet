//! Skills registry API.
//!
//! Serves the git-tracked `skills/` directory as a read-only catalog. Each
//! `SKILL.md` file with YAML frontmatter becomes a skill record.
//!
//! Endpoints:
//!   - `GET /api/skills`            — list all discovered skills
//!   - `GET /api/skills/{*id}`      — full markdown for one skill

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
struct SkillSummary {
    id: String,
    scope: String,
    name: String,
    description: String,
    when_to_invoke: String,
    family: String,
    source: String,
    version: String,
    tools: Vec<String>,
    triggers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SkillDetail {
    id: String,
    scope: String,
    name: String,
    content: String,
}

fn skills_roots() -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Ok(raw) = std::env::var("FORGEFLEET_CONFIG") {
        let path = PathBuf::from(raw);
        if let Some(parent) = path.parent() {
            let candidate = parent.join("skills");
            if let Ok(canonical) = candidate.canonicalize() {
                if seen.insert(canonical.clone()) {
                    roots.push(canonical);
                }
            } else if seen.insert(candidate.clone()) {
                roots.push(candidate);
            }
        }
    }

    if let Ok((_, path)) = ff_core::config::load_config_auto() {
        if let Some(parent) = path.parent() {
            let candidate = parent.join("skills");
            if let Ok(canonical) = candidate.canonicalize() {
                if seen.insert(canonical.clone()) {
                    roots.push(canonical);
                }
            } else if seen.insert(candidate.clone()) {
                roots.push(candidate);
            }
        }
    }

    if let Ok(cd) = std::env::current_dir() {
        let candidate = cd.join("skills");
        if let Ok(canonical) = candidate.canonicalize() {
            if seen.insert(canonical.clone()) {
                roots.push(canonical);
            }
        } else if seen.insert(candidate.clone()) {
            roots.push(candidate);
        }
    }

    roots
}

fn discover_skill_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
                out.push(path);
            }
        }
    }
    out
}

fn parse_frontmatter(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(body) = text.strip_prefix("---") else {
        return map;
    };
    let Some(end) = body.find("\n---") else {
        return map;
    };
    let fm = &body[..end];
    let mut lines = fm.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let val = val.trim();

        if val == "|" {
            let mut block = String::new();
            let mut first_indent: Option<usize> = None;
            while let Some(&next) = lines.peek() {
                if next.trim().is_empty() {
                    if !block.is_empty() {
                        block.push('\n');
                    }
                    lines.next();
                    continue;
                }
                // Heuristic: a new top-level key is an unindented `word:` that is not a list item.
                let indent = next.len() - next.trim_start().len();
                let trim = next.trim_start();
                if indent == 0 && trim.contains(':') && !trim.starts_with("- ") {
                    break;
                }
                if first_indent.is_none() {
                    first_indent = Some(indent);
                }
                let base = first_indent.unwrap_or(0).min(indent);
                let stripped = next.get(base..).unwrap_or(next);
                if !block.is_empty() {
                    block.push('\n');
                }
                block.push_str(stripped);
                lines.next();
            }
            map.insert(key, block.trim().to_string());
        } else {
            map.insert(key, val.to_string());
        }
    }

    map
}

fn parse_list(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| {
            let trim = line.trim_start();
            trim.strip_prefix("- ")
                .or_else(|| trim.strip_prefix("-"))
                .map(|s| s.trim().to_string())
        })
        .collect()
}

fn summarize(path: &Path, root: &Path) -> Option<SkillSummary> {
    let relative = path.strip_prefix(root).ok()?;
    let id = relative
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    if id.is_empty() {
        return None;
    }
    let scope = relative
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default();

    let content = std::fs::read_to_string(path).ok()?;
    let fm = parse_frontmatter(&content);
    let name = fm.get("name").cloned().unwrap_or_else(|| {
        relative
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let tools = fm.get("tools").map(|s| parse_list(s)).unwrap_or_default();
    let triggers = fm.get("triggers").map(|s| parse_list(s)).unwrap_or_default();

    Some(SkillSummary {
        id,
        scope,
        name,
        description: fm.get("description").cloned().unwrap_or_default(),
        when_to_invoke: fm.get("when-to-invoke").cloned().unwrap_or_default(),
        family: fm.get("family").cloned().unwrap_or_default(),
        source: fm.get("source").cloned().unwrap_or_default(),
        version: fm.get("version").cloned().unwrap_or_default(),
        tools,
        triggers,
    })
}

/// GET /api/skills — list all discovered skills.
pub async fn list_skills() -> impl IntoResponse {
    let mut skills: Vec<SkillSummary> = Vec::new();
    let mut seen = HashSet::new();
    for root in skills_roots() {
        for path in discover_skill_files(&root) {
            let Some(summary) = summarize(&path, &root) else {
                continue;
            };
            if !seen.insert(summary.id.clone()) {
                continue;
            }
            skills.push(summary);
        }
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    Json(json!({ "skills": skills }))
}

/// GET /api/skills/{*id} — full markdown for one skill.
pub async fn get_skill(axum::extract::Path(id): axum::extract::Path<String>) -> impl IntoResponse {
    let id_normalized = id.replace('\\', "/").trim_start_matches('/').to_string();
    for root in skills_roots() {
        let path = root.join(&id_normalized).join("SKILL.md");
        if let Ok(content) = std::fs::read_to_string(&path) {
            let relative = path.strip_prefix(&root).unwrap_or(&path);
            let scope = relative
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .unwrap_or_default();
            let detail = SkillDetail {
                id: id_normalized.clone(),
                scope,
                name: content
                    .lines()
                    .find(|l| l.starts_with("# "))
                    .map(|l| l.trim_start_matches("# ").trim().to_string())
                    .unwrap_or_else(|| id_normalized.clone()),
                content,
            };
            return (StatusCode::OK, Json(json!(detail)));
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "skill not found" })),
    )
}
