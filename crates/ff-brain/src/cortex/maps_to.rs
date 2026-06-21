//! Cross-corpus lineage matcher for Cortex nodes.
//!
//! This is a standalone pass, not a per-corpus extractor: it compares already
//! indexed Cortex nodes across two corpora and writes `maps_to` edges.

use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::cmp::Ordering;
use uuid::Uuid;

const MIN_CONFIDENCE: f32 = 0.6;

#[derive(Debug, Clone)]
struct LineageNode {
    id: Uuid,
    path: String,
    title: String,
    node_type: String,
    leaf: String,
    shape: Shape,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Shape {
    column_count: Option<i64>,
    column_type: Option<String>,
    nullable: Option<bool>,
    default: Option<String>,
    check: Option<String>,
    call_fanout: Option<i64>,
    caller_fanin: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LineageGap {
    pub node_type: String,
    pub title: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LineageReport {
    pub from: String,
    pub to: String,
    pub mapped: usize,
    pub unmapped: usize,
    pub total: usize,
    pub sample_gaps: Vec<LineageGap>,
}

/// Match db:table, db:column, and code:function nodes from `old_slug` to
/// `new_slug`, emitting one `maps_to` edge per old node. Returns edges written.
pub async fn map_corpora(pool: &PgPool, old_slug: &str, new_slug: &str) -> Result<usize> {
    let old_nodes = load_nodes(pool, old_slug).await?;
    let new_nodes = load_nodes(pool, new_slug).await?;
    let mut written = 0usize;

    for old in &old_nodes {
        if let Some(matched) = best_match(old, &new_nodes) {
            if matched.confidence >= MIN_CONFIDENCE {
                upsert_maps_to_edge(
                    pool,
                    old.id,
                    matched.node.id,
                    matched.confidence,
                    matched.method,
                    json!({
                        "from": old_slug,
                        "to": new_slug,
                        "old_path": old.path,
                        "new_path": matched.node.path,
                        "old_title": old.title,
                        "new_title": matched.node.title,
                        "node_type": old.node_type,
                        "shape_similarity": matched.shape_similarity,
                    }),
                )
                .await?;
                written += 1;
                continue;
            }
        }

        let marker = upsert_unmapped_marker(pool, new_slug, &old.node_type).await?;
        upsert_maps_to_edge(
            pool,
            old.id,
            marker,
            0.0,
            "unmapped",
            json!({
                "from": old_slug,
                "to": new_slug,
                "old_path": old.path,
                "old_title": old.title,
                "node_type": old.node_type,
            }),
        )
        .await?;
        written += 1;
    }

    Ok(written)
}

pub async fn lineage_report(
    pool: &PgPool,
    old_slug: &str,
    new_slug: &str,
    sample_limit: i64,
) -> Result<LineageReport> {
    let marker_prefix = format!("mapsto://{new_slug}/unmapped/");
    let rows = sqlx::query(
        r#"
        SELECT old.node_type,
               old.title,
               old.path,
               COALESCE(edge.confidence, 0.0) AS confidence,
               new.path AS new_path
          FROM brain_vault_nodes old
          LEFT JOIN brain_vault_edges edge
            ON edge.src_id = old.id
           AND edge.edge_type = 'maps_to'
           AND edge.provenance = 'cortex-maps-to'
          LEFT JOIN brain_vault_nodes new
            ON new.id = edge.dst_id
         WHERE old.project = $1
           AND old.valid_until IS NULL
           AND old.node_type IN ('db:table', 'db:column', 'code:function')
        "#,
    )
    .bind(old_slug)
    .fetch_all(pool)
    .await?;

    let mut report = LineageReport {
        from: old_slug.to_string(),
        to: new_slug.to_string(),
        total: rows.len(),
        ..LineageReport::default()
    };

    for row in rows {
        let confidence: f32 = row.get("confidence");
        let new_path: Option<String> = row.try_get("new_path").ok().flatten();
        let unmapped = confidence < MIN_CONFIDENCE
            || new_path
                .as_deref()
                .map(|path| path.starts_with(&marker_prefix))
                .unwrap_or(true);
        if unmapped {
            report.unmapped += 1;
            if report.sample_gaps.len() < sample_limit as usize {
                report.sample_gaps.push(LineageGap {
                    node_type: row.get("node_type"),
                    title: row.get("title"),
                    path: row.get("path"),
                });
            }
        } else {
            report.mapped += 1;
        }
    }

    Ok(report)
}

struct Candidate<'a> {
    node: &'a LineageNode,
    confidence: f32,
    method: &'static str,
    shape_similarity: f32,
}

fn best_match<'a>(old: &LineageNode, candidates: &'a [LineageNode]) -> Option<Candidate<'a>> {
    candidates
        .iter()
        .filter(|new| new.node_type == old.node_type)
        .filter_map(|new| {
            let (confidence, method) = if new.title == old.title {
                (0.95, "exact_title")
            } else if new.leaf == old.leaf {
                (0.7, "leaf_name")
            } else {
                return None;
            };
            Some(Candidate {
                node: new,
                confidence,
                method,
                shape_similarity: shape_similarity(&old.shape, &new.shape),
            })
        })
        .max_by(compare_candidate)
}

fn compare_candidate(left: &Candidate<'_>, right: &Candidate<'_>) -> Ordering {
    left.confidence
        .partial_cmp(&right.confidence)
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            left.shape_similarity
                .partial_cmp(&right.shape_similarity)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| right.node.title.cmp(&left.node.title))
}

fn shape_similarity(old: &Shape, new: &Shape) -> f32 {
    let mut total = 0usize;
    let mut matches = 0usize;
    compare_field(
        &old.column_count,
        &new.column_count,
        &mut total,
        &mut matches,
    );
    compare_field(&old.column_type, &new.column_type, &mut total, &mut matches);
    compare_field(&old.nullable, &new.nullable, &mut total, &mut matches);
    compare_field(&old.default, &new.default, &mut total, &mut matches);
    compare_field(&old.check, &new.check, &mut total, &mut matches);
    compare_field(&old.call_fanout, &new.call_fanout, &mut total, &mut matches);
    compare_field(
        &old.caller_fanin,
        &new.caller_fanin,
        &mut total,
        &mut matches,
    );
    if total == 0 {
        1.0
    } else {
        matches as f32 / total as f32
    }
}

fn compare_field<T: PartialEq>(
    left: &Option<T>,
    right: &Option<T>,
    total: &mut usize,
    matches: &mut usize,
) {
    if left.is_some() || right.is_some() {
        *total += 1;
        if left == right {
            *matches += 1;
        }
    }
}

async fn load_nodes(pool: &PgPool, slug: &str) -> Result<Vec<LineageNode>> {
    let rows = sqlx::query(
        r#"
        SELECT n.id,
               n.path,
               n.title,
               n.node_type,
               CASE
                 WHEN n.node_type = 'db:table' THEN (
                   SELECT count(*)
                     FROM brain_vault_edges e
                     JOIN brain_vault_nodes c ON c.id = e.dst_id
                    WHERE e.src_id = n.id
                      AND e.edge_type = 'has_column'
                      AND c.node_type = 'db:column'
                      AND c.valid_until IS NULL
                 )
                 ELSE NULL
               END AS column_count,
               CASE
                 WHEN n.node_type = 'db:column' THEN (
                   SELECT e.evidence->>'type'
                     FROM brain_vault_edges e
                    WHERE e.dst_id = n.id
                      AND e.edge_type = 'has_column'
                    LIMIT 1
                 )
                 ELSE NULL
               END AS column_type,
               CASE
                 WHEN n.node_type = 'db:column' THEN (
                   SELECT (e.evidence->>'nullable')::boolean
                     FROM brain_vault_edges e
                    WHERE e.dst_id = n.id
                      AND e.edge_type = 'has_column'
                    LIMIT 1
                 )
                 ELSE NULL
               END AS nullable,
               CASE
                 WHEN n.node_type = 'db:column' THEN (
                   SELECT e.evidence->>'default'
                     FROM brain_vault_edges e
                    WHERE e.dst_id = n.id
                      AND e.edge_type = 'has_column'
                    LIMIT 1
                 )
                 ELSE NULL
               END AS default_value,
               CASE
                 WHEN n.node_type = 'db:column' THEN (
                   SELECT e.evidence->>'check'
                     FROM brain_vault_edges e
                    WHERE e.dst_id = n.id
                      AND e.edge_type = 'has_column'
                    LIMIT 1
                 )
                 ELSE NULL
               END AS check_value,
               CASE
                 WHEN n.node_type = 'code:function' THEN (
                   SELECT count(*)
                     FROM brain_vault_edges e
                    WHERE e.src_id = n.id
                      AND e.edge_type = 'calls'
                 )
                 ELSE NULL
               END AS call_fanout,
               CASE
                 WHEN n.node_type = 'code:function' THEN (
                   SELECT count(*)
                     FROM brain_vault_edges e
                    WHERE e.dst_id = n.id
                      AND e.edge_type = 'calls'
                 )
                 ELSE NULL
               END AS caller_fanin
          FROM brain_vault_nodes n
         WHERE n.project = $1
           AND n.valid_until IS NULL
           AND n.node_type IN ('db:table', 'db:column', 'code:function')
         ORDER BY n.node_type, n.title
        "#,
    )
    .bind(slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let title: String = row.get("title");
            LineageNode {
                id: row.get("id"),
                path: row.get("path"),
                leaf: leaf_name(&title),
                title,
                node_type: row.get("node_type"),
                shape: Shape {
                    column_count: row.try_get("column_count").ok().flatten(),
                    column_type: row.try_get("column_type").ok().flatten(),
                    nullable: row.try_get("nullable").ok().flatten(),
                    default: row.try_get("default_value").ok().flatten(),
                    check: row.try_get("check_value").ok().flatten(),
                    call_fanout: row.try_get("call_fanout").ok().flatten(),
                    caller_fanin: row.try_get("caller_fanin").ok().flatten(),
                },
            }
        })
        .collect())
}

fn leaf_name(title: &str) -> String {
    title
        .rsplit([':', '.', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or(title)
        .to_string()
}

async fn upsert_unmapped_marker(pool: &PgPool, slug: &str, node_type: &str) -> Result<Uuid> {
    let path = format!("mapsto://{slug}/unmapped/{node_type}");
    let title = format!("unmapped {node_type}");
    Ok(sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_nodes
            (path, title, node_type, project, content_hash, confidence, provenance)
        VALUES ($1, $2, 'lineage:unmapped', $3, $1, 1.0, 'cortex-maps-to')
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
    .bind(slug)
    .fetch_one(pool)
    .await?)
}

async fn upsert_maps_to_edge(
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
        VALUES ($1, $2, 'maps_to', 'cortex-maps-to', $3, $4, $5)
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance,
              method = EXCLUDED.method,
              evidence = EXCLUDED.evidence
        "#,
    )
    .bind(src)
    .bind(dst)
    .bind(confidence)
    .bind(method)
    .bind(evidence)
    .execute(pool)
    .await?;
    Ok(())
}
