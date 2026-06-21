//! Standalone PM-to-Cortex ingestor.
//!
//! This is deliberately separate from the per-corpus code extractors: PM
//! work_items are global operational records, and their links point into any
//! existing Cortex node by path.

use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug)]
struct WorkItem {
    id: Uuid,
    title: String,
    description: Option<String>,
    project_id: Option<String>,
    metadata: Value,
}

pub async fn ingest_pm(pool: &PgPool) -> Result<(usize, usize)> {
    let items = read_work_items(pool).await?;
    let mut node_count = 0usize;
    let mut edge_count = 0usize;

    for item in items {
        let project = item.project_id.as_deref().unwrap_or("pm");
        let pm_path = format!("pm://work_item/{}", item.id);
        let pm_node = upsert_pm_node(pool, &pm_path, &item.title, project).await?;
        node_count += 1;

        let mut linked_paths = Vec::new();
        for path in referenced_paths(&item) {
            if linked_paths.iter().any(|seen| seen == &path) {
                continue;
            }
            if let Some(dst) = lookup_current_node(pool, &path).await? {
                add_tracked_by_edge(pool, pm_node, dst, "path_ref", Some(&path)).await?;
                linked_paths.push(path);
                edge_count += 1;
            }
        }

        if linked_paths.is_empty() {
            if let Some((dst, title)) = best_effort_symbol_link(pool, &item).await? {
                add_tracked_by_edge(pool, pm_node, dst, "symbol_title", Some(&title)).await?;
                edge_count += 1;
            }
        }
    }

    Ok((node_count, edge_count))
}

async fn read_work_items(pool: &PgPool) -> Result<Vec<WorkItem>> {
    let rows = sqlx::query(
        r#"
        SELECT id, title, description, project_id, metadata
          FROM work_items
         ORDER BY created_at ASC, id ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| WorkItem {
            id: row.get("id"),
            title: row.get("title"),
            description: row.try_get("description").ok().flatten(),
            project_id: row.try_get("project_id").ok().flatten(),
            metadata: row.try_get("metadata").unwrap_or_else(|_| json!({})),
        })
        .collect())
}

async fn upsert_pm_node(pool: &PgPool, path: &str, title: &str, project: &str) -> Result<Uuid> {
    let id = sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_nodes
            (path, title, node_type, project, content_hash, confidence, provenance)
        VALUES ($1, $2, 'pm:work_item', $3, $1, 1.0, 'pm-ingest')
        ON CONFLICT (path) DO UPDATE
          SET title = EXCLUDED.title,
              node_type = EXCLUDED.node_type,
              project = EXCLUDED.project,
              content_hash = EXCLUDED.content_hash,
              valid_until = NULL,
              updated_at = NOW(),
              confidence = GREATEST(brain_vault_nodes.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance
        RETURNING id
        "#,
    )
    .bind(path)
    .bind(title)
    .bind(project)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn lookup_current_node(pool: &PgPool, path: &str) -> Result<Option<Uuid>> {
    let id = sqlx::query_scalar(
        "SELECT id FROM brain_vault_nodes WHERE path = $1 AND valid_until IS NULL LIMIT 1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

async fn add_tracked_by_edge(
    pool: &PgPool,
    src: Uuid,
    dst: Uuid,
    method: &str,
    matched: Option<&str>,
) -> Result<()> {
    let evidence = matched.map(|m| json!({ "matched": m }));
    sqlx::query(
        r#"
        INSERT INTO brain_vault_edges
            (src_id, dst_id, edge_type, provenance, confidence, method, evidence)
        VALUES ($1, $2, 'tracked_by', 'pm-ingest', 1.0, $3, $4)
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance,
              method = COALESCE(EXCLUDED.method, brain_vault_edges.method),
              evidence = COALESCE(EXCLUDED.evidence, brain_vault_edges.evidence)
        "#,
    )
    .bind(src)
    .bind(dst)
    .bind(method)
    .bind(evidence.as_ref())
    .execute(pool)
    .await?;
    Ok(())
}

fn referenced_paths(item: &WorkItem) -> Vec<String> {
    let mut out = Vec::new();
    collect_metadata_paths(&item.metadata, &mut out);

    let mut text = String::new();
    if let Some(description) = &item.description {
        text.push_str(description);
        text.push('\n');
    }
    text.push_str(&item.metadata.to_string());

    let re = Regex::new(r#"(?:code|db|https?)://[^\s"'<>)]+"#).expect("valid path regex");
    for cap in re.find_iter(&text) {
        let path = trim_path(cap.as_str());
        if !path.is_empty() {
            out.push(path.to_string());
        }
    }
    out
}

fn collect_metadata_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "node_path" | "path" | "code_path" | "db_path") {
                    if let Some(path) = value.as_str() {
                        out.push(trim_path(path).to_string());
                    }
                }
                collect_metadata_paths(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_metadata_paths(value, out);
            }
        }
        Value::String(s) => {
            let re = Regex::new(r#"(?:code|db|https?)://[^\s"'<>)]+"#).expect("valid path regex");
            for cap in re.find_iter(s) {
                out.push(trim_path(cap.as_str()).to_string());
            }
        }
        _ => {}
    }
}

fn trim_path(path: &str) -> &str {
    path.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '}'))
}

async fn best_effort_symbol_link(pool: &PgPool, item: &WorkItem) -> Result<Option<(Uuid, String)>> {
    let mut haystack = item.title.clone();
    haystack.push('\n');
    if let Some(description) = &item.description {
        haystack.push_str(description);
    }
    if haystack.trim().is_empty() {
        return Ok(None);
    }

    let rows = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT id,
                   title,
                   regexp_replace(title, '^.*::', '') AS leaf
              FROM brain_vault_nodes
             WHERE valid_until IS NULL
               AND node_type LIKE 'code:%'
        )
        SELECT id, title, leaf
          FROM candidates
         WHERE length(leaf) >= 4
           AND position(lower(leaf) in lower($1)) > 0
         ORDER BY length(title) DESC, title ASC
         LIMIT 25
        "#,
    )
    .bind(&haystack)
    .fetch_all(pool)
    .await?;

    let mut matches = Vec::new();
    for row in rows {
        let leaf: String = row.get("leaf");
        if contains_symbol_leaf(&haystack, &leaf) {
            matches.push((row.get("id"), row.get("title")));
        }
    }

    if matches.len() == 1 {
        Ok(matches.pop())
    } else {
        Ok(None)
    }
}

fn contains_symbol_leaf(haystack: &str, leaf: &str) -> bool {
    let haystack = haystack.to_lowercase();
    let leaf = leaf.to_lowercase();
    let mut start = 0usize;
    while let Some(offset) = haystack[start..].find(&leaf) {
        let idx = start + offset;
        let before = haystack[..idx].chars().next_back();
        let after = haystack[idx + leaf.len()..].chars().next();
        if !is_ident_char(before) && !is_ident_char(after) {
            return true;
        }
        start = idx + leaf.len();
    }
    false
}

fn is_ident_char(ch: Option<char>) -> bool {
    ch.map(|c| c.is_ascii_alphanumeric() || c == '_')
        .unwrap_or(false)
}
