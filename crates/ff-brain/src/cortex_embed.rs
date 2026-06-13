//! Cortex semantic-embedding pass.
//!
//! Cortex (`ff cortex index`) builds the STRUCTURAL graph — `code:*` / `doc:*` /
//! `data:*` / `image:*` nodes in `brain_vault_nodes` plus their edges. It never
//! populated the `embedding vector(1024)` column, so semantic search over the
//! Cortex graph (`vector_search` / `hybrid_search`) returned nothing: those
//! helpers embed the *query* and compare against stored node vectors, of which
//! there were zero.
//!
//! This module fills that column. It discovers a live fleet embedding endpoint
//! (bge-m3, 1024-dim) via [`fleet_embedding_client`] and batch-embeds the node
//! identity text (`<kind> <fully-qualified-title> [tags]`). It deliberately
//! ABORTS when no real endpoint exists rather than fall through to the hash
//! stub — storing deterministic-noise vectors would silently poison search.
//!
//! Community detection ([`crate::detect_communities`]) is a separate, cheap
//! graph pass the caller runs afterwards; the two together make the Cortex
//! graph navigable the way graphify / code-review-graph are.

use sqlx::{PgPool, Row};

use crate::embeddings::fleet_embedding_client;
use crate::vector_search::embedding_to_pgvector;

/// How many node texts to send per `/v1/embeddings` request. bge-m3 handles a
/// few hundred short strings comfortably; 64 keeps each request small and the
/// progress log lively without thrashing the endpoint.
const EMBED_BATCH: usize = 64;

/// Node types Cortex owns — the ones this pass embeds. Vault notes / facts are
/// embedded by their own ingestion path and are left alone here.
const CORTEX_PREFIXES: &[&str] = &["code:", "doc:", "data:", "image:"];

/// Outcome of an embedding pass.
#[derive(Debug, Default, Clone)]
pub struct EmbedStats {
    /// Nodes that got a fresh embedding stored this pass.
    pub embedded: usize,
    /// Nodes whose embedding call failed (left NULL for a later pass).
    pub failed: usize,
    /// Cortex nodes still NULL after the pass (e.g. a `--max` cap was hit).
    pub remaining: i64,
}

/// Build the text we embed for one node. The fully-qualified title
/// (`ff_pulse::heartbeat::start`) already encodes crate + module + symbol, so a
/// short kind prefix + tags is enough to make symbol-name semantic search work
/// ("where do we publish heartbeats" → `publish_beat`). Doc sections carry a
/// human title which embeds directly.
fn embed_text(node_type: &str, title: &str, tags: &[String]) -> String {
    let kind = node_type.split(':').next_back().unwrap_or(node_type);
    if tags.is_empty() {
        format!("{kind} {title}")
    } else {
        format!("{kind} {title} [{}]", tags.join(", "))
    }
}

/// Count Cortex nodes still missing an embedding. When `corpus` is `Some`, only
/// nodes whose `project` matches that corpus slug are counted (NULL = fleet-wide).
async fn remaining_unembedded(pool: &PgPool, corpus: Option<&str>) -> Result<i64, String> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM brain_vault_nodes
          WHERE valid_until IS NULL AND embedding IS NULL
            AND (node_type LIKE 'code:%' OR node_type LIKE 'doc:%'
              OR node_type LIKE 'data:%' OR node_type LIKE 'image:%')
            AND ($1::text IS NULL OR project = $1)",
    )
    .bind(corpus)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count unembedded: {e}"))?;
    Ok(n)
}

/// Embed every Cortex node whose `embedding` is NULL, in batches, until the
/// graph is fully embedded or `max` nodes have been processed this run.
///
/// `progress` is invoked after each batch with `(embedded_so_far, remaining)`
/// so the CLI can render a live counter. Returns once no unembedded Cortex
/// nodes remain (or the cap is hit). Aborts immediately if the fleet has no
/// healthy embedding endpoint — by design, to avoid persisting hash-stub noise.
///
/// `corpus` scopes the pass to a single corpus slug (the `project` column). The
/// fleet-wide pass embeds by `updated_at` order, so a freshly-reindexed corpus
/// (newest rows) is embedded LAST — passing its slug here lets an agent embed
/// the repo it's working in first, instead of waiting behind every other corpus.
pub async fn embed_cortex_nodes<F>(
    pool: &PgPool,
    max: Option<usize>,
    corpus: Option<&str>,
    mut progress: F,
) -> Result<EmbedStats, String>
where
    F: FnMut(usize, i64),
{
    let client = fleet_embedding_client(pool).await.ok_or_else(|| {
        "no healthy fleet embedding endpoint — load one with \
         `ff model load <bge-m3-lib-id>` (needs preferred_workloads=embedding)"
            .to_string()
    })?;

    let mut stats = EmbedStats::default();

    loop {
        if let Some(cap) = max {
            if stats.embedded + stats.failed >= cap {
                break;
            }
        }

        // Pull a batch of still-NULL Cortex nodes. The WHERE clause is stable
        // across iterations because each row we touch is set non-NULL (or
        // counted as failed and retried next run), so the window advances.
        let rows = sqlx::query(
            "SELECT id, node_type, title, tags FROM brain_vault_nodes
              WHERE valid_until IS NULL AND embedding IS NULL
                AND (node_type LIKE 'code:%' OR node_type LIKE 'doc:%'
                  OR node_type LIKE 'data:%' OR node_type LIKE 'image:%')
                AND ($2::text IS NULL OR project = $2)
              ORDER BY updated_at
              LIMIT $1",
        )
        .bind(EMBED_BATCH as i64)
        .bind(corpus)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("fetch unembedded batch: {e}"))?;

        if rows.is_empty() {
            break;
        }

        // Decode the batch.
        let mut ids: Vec<uuid::Uuid> = Vec::with_capacity(rows.len());
        let mut texts: Vec<String> = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: uuid::Uuid = r.get("id");
            let node_type: String = r.try_get("node_type").unwrap_or_default();
            let title: String = r.try_get("title").unwrap_or_default();
            let tags: Vec<String> = r.try_get("tags").unwrap_or_default();
            ids.push(id);
            texts.push(embed_text(&node_type, &title, &tags));
        }

        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = match client.embed_batch(&text_refs).await {
            Ok(v) if v.len() == ids.len() => v,
            Ok(v) => {
                // Length mismatch — count the batch as failed and continue so a
                // single bad batch can't wedge the whole pass.
                tracing::warn!(
                    expected = ids.len(),
                    got = v.len(),
                    "embedding batch length mismatch; skipping batch"
                );
                stats.failed += ids.len();
                continue;
            }
            Err(e) => {
                tracing::warn!("embedding batch failed: {e}");
                stats.failed += ids.len();
                // A persistent endpoint failure would loop forever re-fetching
                // the same rows; bail out so the caller sees partial progress.
                break;
            }
        };

        for (id, vec) in ids.iter().zip(vectors.iter()) {
            if vec.is_empty() {
                stats.failed += 1;
                continue;
            }
            let pgvec = embedding_to_pgvector(vec);
            match sqlx::query(
                "UPDATE brain_vault_nodes SET embedding = $1::vector, updated_at = NOW() WHERE id = $2",
            )
            .bind(&pgvec)
            .bind(id)
            .execute(pool)
            .await
            {
                Ok(_) => stats.embedded += 1,
                Err(e) => {
                    tracing::warn!(node = %id, "store embedding failed: {e}");
                    stats.failed += 1;
                }
            }
        }

        let remaining = remaining_unembedded(pool, corpus).await.unwrap_or(-1);
        progress(stats.embedded, remaining);
    }

    stats.remaining = remaining_unembedded(pool, corpus).await.unwrap_or(-1);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::embed_text;

    #[test]
    fn embed_text_without_tags_is_kind_plus_title() {
        // Leaf kind only (after the last ':'), then the fully-qualified title.
        let t = embed_text("code:function", "ff_pulse::heartbeat::start", &[]);
        assert_eq!(t, "function ff_pulse::heartbeat::start");
    }

    #[test]
    fn embed_text_with_tags_appends_bracketed_list() {
        let t = embed_text(
            "code:function",
            "ff_db::pg_reprofile_candidates",
            &["agent".to_string(), "tool_calling".to_string()],
        );
        assert_eq!(
            t,
            "function ff_db::pg_reprofile_candidates [agent, tool_calling]"
        );
    }

    #[test]
    fn embed_text_unprefixed_node_type_kept_whole() {
        // A node_type with no ':' falls back to itself as the kind.
        let t = embed_text("doc", "Cortex roadmap", &[]);
        assert_eq!(t, "doc Cortex roadmap");
    }
}
