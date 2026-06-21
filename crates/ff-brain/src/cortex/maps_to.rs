//! Cross-corpus Cortex lineage matcher.
//!
//! This is a standalone graph pass: it compares already-indexed Cortex nodes in
//! two corpora and writes `maps_to` edges from the old corpus to the new corpus.

use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

const PROVENANCE: &str = "cortex-maps-to";
const MIN_CONFIDENCE: f32 = 0.6;

#[derive(Debug, Clone)]
struct LineageNode {
    id: Uuid,
    path: String,
    title: String,
    node_type: String,
    leaf: String,
    shape: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct MatchChoice<'a> {
    node: &'a LineageNode,
    confidence: f32,
    method: &'static str,
    shape_score: f32,
}

#[derive(Debug, Clone)]
pub struct LineageSummary {
    pub mapped: i64,
    pub unmapped: i64,
}

#[derive(Debug, Clone)]
pub struct LineageGap {
    pub title: String,
    pub path: String,
    pub node_type: String,
}

/// Match db:table/db:column/code:function nodes between two corpora and emit
/// `maps_to` edges. Returns the number of old-corpus nodes processed.
pub async fn map_corpora(pool: &PgPool, old_slug: &str, new_slug: &str) -> Result<usize> {
    let old_nodes = load_nodes(pool, old_slug).await?;
    let new_nodes = load_nodes(pool, new_slug).await?;

    clear_previous_edges(pool, old_slug).await?;

    let mut by_type: HashMap<&str, Vec<&LineageNode>> = HashMap::new();
    for node in &new_nodes {
        by_type.entry(&node.node_type).or_default().push(node);
    }

    for old in &old_nodes {
        let candidates = by_type
            .get(old.node_type.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if let Some(best) = best_match(old, candidates) {
            add_maps_to_edge(
                pool,
                old.id,
                best.node.id,
                best.confidence,
                best.method,
                json!({
                    "from": old_slug,
                    "to": new_slug,
                    "old_path": old.path,
                    "new_path": best.node.path,
                    "old_title": old.title,
                    "new_title": best.node.title,
                    "shape_score": best.shape_score,
                }),
            )
            .await?;
        } else {
            let marker = upsert_unmapped_marker(pool, new_slug, old).await?;
            add_maps_to_edge(
                pool,
                old.id,
                marker,
                0.0,
                "UNMAPPED",
                json!({
                    "from": old_slug,
                    "to": new_slug,
                    "old_path": old.path,
                    "old_title": old.title,
                    "reason": "no counterpart over confidence threshold",
                }),
            )
            .await?;
        }
    }

    Ok(old_nodes.len())
}

pub async fn lineage_summary(
    pool: &PgPool,
    old_slug: &str,
    new_slug: &str,
) -> Result<LineageSummary> {
    let mapped = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = 'maps_to'
           AND e.provenance = $3
           AND src.project = $1
           AND dst.project = $2
           AND dst.node_type <> 'cortex:unmapped'
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .bind(old_slug)
    .bind(new_slug)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;

    let unmapped = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = 'maps_to'
           AND e.provenance = $3
           AND src.project = $1
           AND dst.project = $2
           AND dst.node_type = 'cortex:unmapped'
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .bind(old_slug)
    .bind(new_slug)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;

    Ok(LineageSummary { mapped, unmapped })
}

pub async fn sample_gaps(
    pool: &PgPool,
    old_slug: &str,
    new_slug: &str,
    limit: i64,
) -> Result<Vec<LineageGap>> {
    let rows = sqlx::query(
        r#"
        SELECT src.title, src.path, src.node_type
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = 'maps_to'
           AND e.provenance = $3
           AND src.project = $1
           AND dst.project = $2
           AND dst.node_type = 'cortex:unmapped'
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
         ORDER BY src.node_type COLLATE "C", src.title COLLATE "C"
         LIMIT $4
        "#,
    )
    .bind(old_slug)
    .bind(new_slug)
    .bind(PROVENANCE)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| LineageGap {
            title: row.get("title"),
            path: row.get("path"),
            node_type: row.get("node_type"),
        })
        .collect())
}

async fn load_nodes(pool: &PgPool, corpus: &str) -> Result<Vec<LineageNode>> {
    let rows = sqlx::query(
        r#"
        SELECT id, path, title, node_type
          FROM brain_vault_nodes
         WHERE project = $1
           AND valid_until IS NULL
           AND node_type IN ('db:table', 'db:column', 'code:function')
         ORDER BY node_type COLLATE "C", title COLLATE "C", path COLLATE "C"
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    let table_columns = load_table_columns(pool, corpus).await?;
    let column_parents = load_column_parents(pool, corpus).await?;
    let function_callees = load_function_callees(pool, corpus).await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let id = row.get("id");
            let title: String = row.get("title");
            let node_type: String = row.get("node_type");
            let shape = match node_type.as_str() {
                "db:table" => table_columns.get(&id).cloned().unwrap_or_default(),
                "db:column" => column_parents.get(&id).cloned().unwrap_or_default(),
                "code:function" => function_callees.get(&id).cloned().unwrap_or_default(),
                _ => BTreeSet::new(),
            };
            LineageNode {
                id,
                path: row.get("path"),
                leaf: leaf_name(&title).to_string(),
                title,
                node_type,
                shape,
            }
        })
        .collect())
}

async fn load_table_columns(
    pool: &PgPool,
    corpus: &str,
) -> Result<HashMap<Uuid, BTreeSet<String>>> {
    let rows = sqlx::query(
        r#"
        SELECT t.id AS table_id, c.title AS column_title
          FROM brain_vault_edges e
          JOIN brain_vault_nodes t ON t.id = e.src_id
          JOIN brain_vault_nodes c ON c.id = e.dst_id
         WHERE e.edge_type = 'has_column'
           AND t.project = $1
           AND c.project = $1
           AND t.node_type = 'db:table'
           AND c.node_type = 'db:column'
           AND t.valid_until IS NULL
           AND c.valid_until IS NULL
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    let mut out: HashMap<Uuid, BTreeSet<String>> = HashMap::new();
    for row in rows {
        let table_id: Uuid = row.get("table_id");
        let title: String = row.get("column_title");
        out.entry(table_id)
            .or_default()
            .insert(leaf_name(&title).to_ascii_lowercase());
    }
    Ok(out)
}

async fn load_column_parents(
    pool: &PgPool,
    corpus: &str,
) -> Result<HashMap<Uuid, BTreeSet<String>>> {
    let rows = sqlx::query(
        r#"
        SELECT c.id AS column_id, t.title AS table_title
          FROM brain_vault_edges e
          JOIN brain_vault_nodes t ON t.id = e.src_id
          JOIN brain_vault_nodes c ON c.id = e.dst_id
         WHERE e.edge_type = 'has_column'
           AND t.project = $1
           AND c.project = $1
           AND t.node_type = 'db:table'
           AND c.node_type = 'db:column'
           AND t.valid_until IS NULL
           AND c.valid_until IS NULL
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    let mut out: HashMap<Uuid, BTreeSet<String>> = HashMap::new();
    for row in rows {
        let column_id: Uuid = row.get("column_id");
        let title: String = row.get("table_title");
        out.entry(column_id)
            .or_default()
            .insert(leaf_name(&title).to_ascii_lowercase());
    }
    Ok(out)
}

async fn load_function_callees(
    pool: &PgPool,
    corpus: &str,
) -> Result<HashMap<Uuid, BTreeSet<String>>> {
    let rows = sqlx::query(
        r#"
        SELECT src.id AS function_id, dst.title AS callee_title
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = 'calls'
           AND src.project = $1
           AND dst.project = $1
           AND src.node_type = 'code:function'
           AND dst.node_type LIKE 'code:%'
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    let mut out: HashMap<Uuid, BTreeSet<String>> = HashMap::new();
    for row in rows {
        let function_id: Uuid = row.get("function_id");
        let title: String = row.get("callee_title");
        out.entry(function_id)
            .or_default()
            .insert(leaf_name(&title).to_ascii_lowercase());
    }
    Ok(out)
}

fn best_match<'a>(old: &LineageNode, candidates: &'a [&'a LineageNode]) -> Option<MatchChoice<'a>> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let (confidence, method) = if old.title.eq_ignore_ascii_case(&candidate.title) {
                (0.95, "EXACT_TITLE")
            } else if old.leaf.eq_ignore_ascii_case(&candidate.leaf) {
                (0.7, "LEAF_NAME")
            } else {
                return None;
            };
            if confidence < MIN_CONFIDENCE {
                return None;
            }
            Some(MatchChoice {
                node: candidate,
                confidence,
                method,
                shape_score: shape_score(&old.shape, &candidate.shape),
            })
        })
        .max_by(|a, b| {
            a.confidence
                .total_cmp(&b.confidence)
                .then_with(|| a.shape_score.total_cmp(&b.shape_score))
                .then_with(|| b.node.title.cmp(&a.node.title))
        })
}

async fn clear_previous_edges(pool: &PgPool, old_slug: &str) -> Result<()> {
    sqlx::query(
        r#"
        DELETE FROM brain_vault_edges e
         USING brain_vault_nodes src
         WHERE e.src_id = src.id
           AND e.edge_type = 'maps_to'
           AND e.provenance = $2
           AND src.project = $1
           AND src.node_type IN ('db:table', 'db:column', 'code:function')
        "#,
    )
    .bind(old_slug)
    .bind(PROVENANCE)
    .execute(pool)
    .await?;
    Ok(())
}

async fn upsert_unmapped_marker(pool: &PgPool, new_slug: &str, old: &LineageNode) -> Result<Uuid> {
    let path = format!(
        "cortex://lineage/{}/unmapped/{}",
        new_slug,
        stable_digest(&old.path)
    );
    let title = format!("unmapped: {}", old.title);
    let id = sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_nodes
            (path, title, node_type, project, content_hash, confidence, provenance)
        VALUES ($1, $2, 'cortex:unmapped', $3, $1, 1.0, $4)
        ON CONFLICT (path) DO UPDATE
          SET title = EXCLUDED.title,
              node_type = EXCLUDED.node_type,
              project = EXCLUDED.project,
              content_hash = EXCLUDED.content_hash,
              valid_until = NULL,
              updated_at = NOW(),
              confidence = EXCLUDED.confidence,
              provenance = EXCLUDED.provenance
        RETURNING id
        "#,
    )
    .bind(path)
    .bind(title)
    .bind(new_slug)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn add_maps_to_edge(
    pool: &PgPool,
    src: Uuid,
    dst: Uuid,
    confidence: f32,
    method: &str,
    evidence: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO brain_vault_edges
            (src_id, dst_id, edge_type, provenance, confidence, method, evidence)
        VALUES ($1, $2, 'maps_to', $3, $4, $5, $6)
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = EXCLUDED.confidence,
              provenance = EXCLUDED.provenance,
              method = EXCLUDED.method,
              evidence = EXCLUDED.evidence
        "#,
    )
    .bind(src)
    .bind(dst)
    .bind(PROVENANCE)
    .bind(confidence)
    .bind(method)
    .bind(evidence)
    .execute(pool)
    .await?;
    Ok(())
}

fn leaf_name(title: &str) -> &str {
    title
        .rsplit("::")
        .next()
        .and_then(|s| s.rsplit('.').next())
        .unwrap_or(title)
}

fn shape_score(old: &BTreeSet<String>, new: &BTreeSet<String>) -> f32 {
    if old.is_empty() && new.is_empty() {
        return 1.0;
    }
    let intersection = old.intersection(new).count() as f32;
    let union = old.union(new).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn stable_digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    format!("{:x}", digest)[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_name_handles_code_and_columns() {
        assert_eq!(leaf_name("a::b::load"), "load");
        assert_eq!(leaf_name("users.email"), "email");
        assert_eq!(leaf_name("users"), "users");
    }

    #[test]
    fn shape_score_is_jaccard() {
        let old = BTreeSet::from(["a".to_string(), "b".to_string()]);
        let new = BTreeSet::from(["b".to_string(), "c".to_string()]);
        assert_eq!(shape_score(&old, &new), 1.0 / 3.0);
    }
}
