//! Vector + hybrid search for the vault knowledge graph.

use serde::Serialize;
use sqlx::{PgPool, Row};
use std::collections::HashMap;

use crate::embeddings::generate_embedding_with_pool;

/// A vault node returned from vector/hybrid search.
#[derive(Debug, Serialize)]
pub struct VaultNode {
    pub id: uuid::Uuid,
    pub path: String,
    pub title: String,
    pub node_type: Option<String>,
    pub score: f32,
}

/// Format a Vec<f32> as a pgvector literal string.
pub fn embedding_to_pgvector(vec: &[f32]) -> String {
    let parts: Vec<String> = vec.iter().map(|f| f.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// Vector search: find vault nodes by embedding similarity.
///
/// Uses pgvector `<->` (Euclidean distance) operator.  Results are
/// scored as `1 / (1 + distance)` so higher = more similar.
pub async fn vector_search(query: &str, top_k: i64, pg: &PgPool) -> anyhow::Result<Vec<VaultNode>> {
    let embedding = generate_embedding_with_pool(query, pg).await;
    let embedding_str = embedding_to_pgvector(&embedding);

    // Same graceful-degrade as hybrid_search: when pgvector isn't installed
    // or the column dim doesn't match, return an empty result instead of
    // bubbling a 500 up to /api/brain/vault/search.
    let rows = match sqlx::query(
        r#"
        SELECT id, path, title, node_type,
               embedding <-> $1::vector AS distance
        FROM brain_vault_nodes
        WHERE valid_until IS NULL
          AND embedding IS NOT NULL
        ORDER BY embedding <-> $1::vector
        LIMIT $2
        "#,
    )
    .bind(&embedding_str)
    .bind(top_k)
    .fetch_all(pg)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("vector_search disabled (likely no pgvector or dim mismatch): {e}");
            return Ok(Vec::new());
        }
    };

    let mut results = Vec::new();
    for row in rows {
        // pgvector's `<->` operator returns FLOAT8; sqlx will refuse to
        // decode that into f32. Read as f64 then narrow.
        let distance: f64 = row.get("distance");
        let score = (1.0f32 / (1.0f32 + distance as f32)).min(1.0f32);
        results.push(VaultNode {
            id: row.get("id"),
            path: row.get("path"),
            title: row.get("title"),
            node_type: row.get("node_type"),
            score,
        });
    }
    Ok(results)
}

/// Hybrid search: combine vector similarity with keyword matching.
///
/// - Vector results are fetched with 2× `top_k` and scored by inverse distance.
/// - Keyword results are fetched with 2× `top_k` and scored by a flat match bonus.
/// - Combined score = vector_score×0.6 + keyword_score×0.4 (boosted when both match).
pub async fn hybrid_search(query: &str, top_k: i64, pg: &PgPool) -> anyhow::Result<Vec<VaultNode>> {
    let embedding = generate_embedding_with_pool(query, pg).await;
    let embedding_str = embedding_to_pgvector(&embedding);
    let pattern = format!("%{}%", query);

    // ── Vector candidates ──────────────────────────────────────────────
    // V78 only adds the `embedding` column when the pgvector extension is
    // present. If pgvector was never installed (or vector dimensionality
    // doesn't match the active embedder), the column query errors out.
    // Degrade to keyword-only rather than 500ing the whole /api/brain/search
    // request — the search still works, just without semantic boost.
    let vector_rows: Vec<sqlx::postgres::PgRow> = match sqlx::query(
        r#"
        SELECT id, path, title, node_type,
               embedding <-> $1::vector AS distance
        FROM brain_vault_nodes
        WHERE valid_until IS NULL
          AND embedding IS NOT NULL
        ORDER BY embedding <-> $1::vector
        LIMIT $2
        "#,
    )
    .bind(&embedding_str)
    .bind(top_k * 2)
    .fetch_all(pg)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                "vector search disabled (likely no pgvector or column dim mismatch): {e}. \
                 Falling back to keyword-only hybrid_search."
            );
            Vec::new()
        }
    };

    // ── Keyword candidates ─────────────────────────────────────────────
    let keyword_rows = sqlx::query(
        r#"
        SELECT id, path, title, node_type
        FROM brain_vault_nodes
        WHERE valid_until IS NULL
          AND (title ILIKE $1 OR path ILIKE $1 OR $1 = ANY(tags))
        ORDER BY hits DESC, updated_at DESC
        LIMIT $2
        "#,
    )
    .bind(&pattern)
    .bind(top_k * 2)
    .fetch_all(pg)
    .await?;

    // ── Fuse and re-rank ───────────────────────────────────────────────
    let mut fused: HashMap<uuid::Uuid, (VaultNode, f32)> = HashMap::new();

    for row in vector_rows {
        let id: uuid::Uuid = row.get("id");
        // pgvector returns FLOAT8 — read as f64 and narrow.
        let distance: f64 = row.get("distance");
        let vector_score = (1.0f32 / (1.0f32 + distance as f32)).min(1.0f32);

        let node = VaultNode {
            id,
            path: row.get("path"),
            title: row.get("title"),
            node_type: row.get("node_type"),
            score: vector_score,
        };
        fused.insert(id, (node, vector_score * 0.6));
    }

    for row in keyword_rows {
        let id: uuid::Uuid = row.get("id");
        let keyword_score = 0.5_f32;

        if let Some((_, combined)) = fused.get_mut(&id) {
            *combined += keyword_score * 0.4;
        } else {
            let node = VaultNode {
                id,
                path: row.get("path"),
                title: row.get("title"),
                node_type: row.get("node_type"),
                score: keyword_score,
            };
            fused.insert(id, (node, keyword_score * 0.4));
        }
    }

    let mut results: Vec<VaultNode> = fused
        .into_values()
        .map(|(mut node, score)| {
            node.score = score.min(1.0);
            node
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(top_k as usize);

    Ok(results)
}
