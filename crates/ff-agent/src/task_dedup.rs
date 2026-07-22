//! Instruction hashing and similarity check for `fleet_tasks` dedup.
//!
//! Two-stage duplicate detection for task instructions:
//!
//! 1. **Exact hash match** — [`compute_hash`] derives a stable SHA-256
//!    signature from the normalized instruction text (the same shape stored
//!    in `fleet_tasks.dedup_signature` and enforced single-flight by the
//!    partial unique index `idx_fleet_tasks_dedup_signature`).
//! 2. **Embedding fallback** — when no active task holds the signature,
//!    [`find_similar`] embeds the instruction against recent active tasks of
//!    the same `task_type` via a fleet embedding deployment (OpenAI-style
//!    `/v1/embeddings`, discovered through `ff_db::pg_route_deployments`
//!    exactly like ff-brain's resolver) and returns candidates whose cosine
//!    similarity clears [`EMBEDDING_SIMILARITY_THRESHOLD`].
//!
//! No embedding endpoint (or an unreachable one) degrades gracefully to
//! hash-only matching — near-duplicates are advisory, so noise vectors are
//! never substituted for real embeddings.

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// Minimum cosine similarity for an embedding-stage candidate to count as a
/// potential duplicate.
pub const EMBEDDING_SIMILARITY_THRESHOLD: f32 = 0.85;

/// How many recent active same-type tasks the embedding fallback compares
/// against. Bounds one `/v1/embeddings` batch per lookup.
const EMBEDDING_CANDIDATE_LIMIT: i64 = 50;

/// Statuses that no longer contend for dedup. Terminal `self_heal` rows keep
/// their signature for the re-arm cooldown (see `ha::self_heal`), so the
/// signature column alone is not an "active" filter — status must be checked
/// explicitly.
const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];

/// The slice of a `fleet_tasks` row that dedup reasons about.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Task {
    pub id: Uuid,
    pub task_type: String,
    /// Instruction text — what gets hashed and embedded.
    pub summary: String,
    pub status: String,
    pub dedup_signature: Option<String>,
}

/// How a potential duplicate was matched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MatchKind {
    /// The candidate's `dedup_signature` equals the instruction hash.
    ExactHash,
    /// Cosine similarity of instruction embeddings, in `[threshold, 1]`.
    Embedding(f32),
}

/// One potential duplicate returned by [`find_similar`].
#[derive(Debug, Clone)]
pub struct SimilarTask {
    pub task: Task,
    pub matched_by: MatchKind,
}

/// Whitespace-collapse + lowercase so cosmetic differences (indentation,
/// trailing newlines, casing) hash identically.
fn normalize_instruction(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Stable dedup hash of an instruction text: SHA-256 over the normalized
/// form, hex-encoded. Suitable for `fleet_tasks.dedup_signature`.
pub fn compute_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_instruction(text).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Cosine similarity of two vectors. Mismatched lengths or a zero vector
/// yield `0.0` rather than an error — such pairs are simply "not similar".
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Find potential duplicates of `task` among ACTIVE `fleet_tasks` rows.
///
/// Stage 1 matches the instruction hash (the task's own `dedup_signature`
/// when present, else [`compute_hash`] of its summary) against other active
/// rows' signatures; any hit short-circuits the embedding stage. Stage 2
/// embeds the summary against recent active same-`task_type` rows and keeps
/// candidates at or above [`EMBEDDING_SIMILARITY_THRESHOLD`], best first.
///
/// DB failures surface as `Err`; embedding-infrastructure failures (no
/// deployment, endpoint down) degrade to an empty fallback with a warning.
pub async fn find_similar(pg: &PgPool, task: &Task) -> Result<Vec<SimilarTask>, sqlx::Error> {
    let signature = match task.dedup_signature.as_deref() {
        Some(sig) => sig.to_string(),
        None => compute_hash(&task.summary),
    };

    let exact: Vec<Task> = sqlx::query_as(
        "SELECT id, task_type, summary, status, dedup_signature
           FROM fleet_tasks
          WHERE dedup_signature = $1
            AND id <> $2
            AND status <> ALL($3)",
    )
    .bind(&signature)
    .bind(task.id)
    .bind(&TERMINAL_STATUSES[..])
    .fetch_all(pg)
    .await?;
    if !exact.is_empty() {
        return Ok(exact
            .into_iter()
            .map(|t| SimilarTask {
                task: t,
                matched_by: MatchKind::ExactHash,
            })
            .collect());
    }

    // Hash missed — fall back to embedding similarity over recent active
    // tasks of the same type.
    let candidates: Vec<Task> = sqlx::query_as(
        "SELECT id, task_type, summary, status, dedup_signature
           FROM fleet_tasks
          WHERE task_type = $1
            AND id <> $2
            AND status <> ALL($3)
          ORDER BY created_at DESC
          LIMIT $4",
    )
    .bind(&task.task_type)
    .bind(task.id)
    .bind(&TERMINAL_STATUSES[..])
    .bind(EMBEDDING_CANDIDATE_LIMIT)
    .fetch_all(pg)
    .await?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let Some((endpoint, model)) = resolve_embedding_endpoint(pg).await else {
        tracing::warn!(
            "task dedup: hash missed and no embedding deployment is routable; \
             skipping similarity fallback"
        );
        return Ok(Vec::new());
    };

    let mut texts: Vec<&str> = Vec::with_capacity(candidates.len() + 1);
    texts.push(&task.summary);
    texts.extend(candidates.iter().map(|c| c.summary.as_str()));
    let mut vectors = match embed_batch(&endpoint, &model, &texts).await {
        Ok(v) if v.len() == texts.len() => v,
        Ok(v) => {
            tracing::warn!(
                expected = texts.len(),
                got = v.len(),
                "task dedup: embedding batch returned wrong cardinality; \
                 skipping similarity fallback"
            );
            return Ok(Vec::new());
        }
        Err(e) => {
            tracing::warn!(
                endpoint = %endpoint,
                "task dedup: embedding request failed; skipping similarity fallback: {e}"
            );
            return Ok(Vec::new());
        }
    };
    let target = vectors.remove(0);
    Ok(rank_by_similarity(
        &target,
        candidates.into_iter().zip(vectors),
    ))
}

/// Score candidates against `target`, keep those at or above the threshold,
/// best first. Pure so the ranking is testable without an embedding server.
fn rank_by_similarity(
    target: &[f32],
    candidates: impl IntoIterator<Item = (Task, Vec<f32>)>,
) -> Vec<SimilarTask> {
    let mut similar: Vec<SimilarTask> = candidates
        .into_iter()
        .filter_map(|(task, vector)| {
            let score = cosine_similarity(target, &vector);
            (score >= EMBEDDING_SIMILARITY_THRESHOLD).then_some(SimilarTask {
                task,
                matched_by: MatchKind::Embedding(score),
            })
        })
        .collect();
    similar.sort_by(|a, b| {
        let (MatchKind::Embedding(sa), MatchKind::Embedding(sb)) = (a.matched_by, b.matched_by)
        else {
            return std::cmp::Ordering::Equal;
        };
        sb.total_cmp(&sa)
    });
    similar
}

/// Resolve an embedding endpoint the same way ff-brain does:
/// `FF_EMBEDDING_ENDPOINT`/`FF_EMBEDDING_MODEL` operator override first, then
/// a healthy `fleet_model_deployments` row routed for the "embedding"
/// workload. (`ff-brain` depends on this crate, so its client can't be
/// imported here.)
async fn resolve_embedding_endpoint(pg: &PgPool) -> Option<(String, String)> {
    if let (Ok(endpoint), Ok(model)) = (
        std::env::var("FF_EMBEDDING_ENDPOINT"),
        std::env::var("FF_EMBEDDING_MODEL"),
    ) {
        return Some((endpoint, model));
    }
    let filter = ff_db::RouteFilter {
        workload: Some("embedding".to_string()),
        limit: 1,
        ..Default::default()
    };
    let route = ff_db::pg_route_deployments(pg, &filter)
        .await
        .ok()?
        .into_iter()
        .next()?;
    let model = route
        .catalog_id
        .or(route.catalog_name)
        .unwrap_or_else(|| "embedding".to_string());
    Some((route.endpoint, model))
}

/// POST one OpenAI-style `/v1/embeddings` batch and return one vector per
/// input text, in order.
async fn embed_batch(endpoint: &str, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
    let resp = crate::notifications::SHARED_HTTP
        .post(format!("{endpoint}/v1/embeddings"))
        .json(&json!({ "model": model, "input": texts }))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("embedding request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("embedding server returned {status}: {body}"));
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("embedding response parse failed: {e}"))?;
    let data = payload
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "embedding response missing data array".to_string())?;
    let mut vectors = Vec::with_capacity(data.len());
    for entry in data {
        let vector: Vec<f32> = entry
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| "embedding entry missing embedding array".to_string())?
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        if vector.is_empty() {
            return Err("embedding vector is empty".to_string());
        }
        vectors.push(vector);
    }
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(task_type: &str, summary: &str, signature: Option<&str>) -> Task {
        Task {
            id: Uuid::new_v4(),
            task_type: task_type.to_string(),
            summary: summary.to_string(),
            status: "pending".to_string(),
            dedup_signature: signature.map(str::to_string),
        }
    }

    #[test]
    fn compute_hash_is_deterministic_and_normalized() {
        let a = compute_hash("Deploy   the\n\tgateway service");
        let b = compute_hash("deploy the gateway service");
        assert_eq!(a, b, "whitespace and case must not change the hash");
        assert_eq!(a.len(), 64, "full SHA-256 hex digest");
        assert_ne!(a, compute_hash("deploy the brain service"));
    }

    #[test]
    fn cosine_similarity_basics() {
        assert!((cosine_similarity(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn rank_by_similarity_filters_and_sorts() {
        let target = [1.0, 0.0, 0.0];
        let close = (task("shell", "close", None), vec![0.95f32, 0.05, 0.0]);
        let closer = (task("shell", "closer", None), vec![1.0f32, 0.0, 0.0]);
        let far = (task("shell", "far", None), vec![0.0f32, 1.0, 0.0]);
        let ranked = rank_by_similarity(&target, vec![close, far, closer]);
        assert_eq!(ranked.len(), 2, "below-threshold candidate is dropped");
        assert_eq!(ranked[0].task.summary, "closer");
        assert_eq!(ranked[1].task.summary, "close");
        for hit in &ranked {
            let MatchKind::Embedding(score) = hit.matched_by else {
                panic!("fallback hits must carry an embedding score");
            };
            assert!(score >= EMBEDDING_SIMILARITY_THRESHOLD);
        }
    }

    mod db {
        //! Exercises the hash-match stage against a real Postgres. Skips
        //! (early return) when no FORGEFLEET_POSTGRES_URL /
        //! FORGEFLEET_DATABASE_URL is set — CI has no database.

        use super::*;
        use sqlx::postgres::PgPoolOptions;

        async fn create_temp_db() -> Option<(sqlx::PgPool, sqlx::PgPool, String)> {
            let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
                .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
                .ok()?;
            let (prefix, _) = base_url.rsplit_once('/')?;
            let db_name = format!("ff_task_dedup_{}", Uuid::new_v4().simple());
            let admin = PgPoolOptions::new()
                .max_connections(1)
                .connect(&format!("{prefix}/postgres"))
                .await
                .expect("connect admin db");
            sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
                .execute(&admin)
                .await
                .expect("create temp db");
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect(&format!("{prefix}/{db_name}"))
                .await
                .expect("connect temp db");
            sqlx::raw_sql(
                "CREATE EXTENSION IF NOT EXISTS pgcrypto;
                 CREATE TABLE fleet_tasks (
                     id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                     task_type TEXT NOT NULL,
                     summary TEXT NOT NULL,
                     payload JSONB NOT NULL DEFAULT '{}'::jsonb,
                     priority INT NOT NULL DEFAULT 50,
                     status TEXT NOT NULL DEFAULT 'pending',
                     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                     completed_at TIMESTAMPTZ,
                     task_class TEXT,
                     dedup_signature TEXT
                 );",
            )
            .execute(&pool)
            .await
            .expect("create minimal fleet_tasks schema");
            Some((admin, pool, db_name))
        }

        async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
            pool.close().await;
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
            ))
            .execute(&admin)
            .await
            .expect("drop temp db");
        }

        async fn insert(pool: &sqlx::PgPool, t: &Task) {
            sqlx::query(
                "INSERT INTO fleet_tasks (id, task_type, summary, status, dedup_signature)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(t.id)
            .bind(&t.task_type)
            .bind(&t.summary)
            .bind(&t.status)
            .bind(&t.dedup_signature)
            .execute(pool)
            .await
            .expect("insert task row");
        }

        #[tokio::test]
        async fn exact_hash_match_finds_active_duplicate_only() {
            let Some((admin, pool, db_name)) = create_temp_db().await else {
                eprintln!(
                    "skipping exact_hash_match_finds_active_duplicate_only: \
                     no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
                );
                return;
            };

            let sig = compute_hash("restart the gateway on node priya");
            let active_dup = task("shell", "Restart the gateway on node priya", Some(&sig));
            let mut terminal_dup = task("shell", "restart the gateway on node priya", Some(&sig));
            // Same signature but terminal — must NOT be reported. (A second
            // active row with the signature would trip the prod unique index;
            // the terminal row models a self_heal signature-keeper.)
            terminal_dup.status = "completed".to_string();
            terminal_dup.dedup_signature = None;
            insert(&pool, &active_dup).await;
            insert(&pool, &terminal_dup).await;

            let incoming = task("shell", "Restart  the gateway on node PRIYA", None);
            let hits = find_similar(&pool, &incoming).await.expect("find_similar");
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].task.id, active_dup.id);
            assert_eq!(hits[0].matched_by, MatchKind::ExactHash);

            drop_temp_db(admin, pool, &db_name).await;
        }

        #[tokio::test]
        async fn hash_miss_with_no_candidates_returns_empty() {
            let Some((admin, pool, db_name)) = create_temp_db().await else {
                eprintln!(
                    "skipping hash_miss_with_no_candidates_returns_empty: \
                     no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
                );
                return;
            };

            // Only a different-type row exists, so the fallback candidate set
            // is empty and find_similar returns before touching any embedding
            // infrastructure (the temp db has no fleet_model_deployments).
            insert(&pool, &task("research", "survey embedding models", None)).await;

            let incoming = task("shell", "restart the gateway on node priya", None);
            let hits = find_similar(&pool, &incoming).await.expect("find_similar");
            assert!(hits.is_empty());

            drop_temp_db(admin, pool, &db_name).await;
        }
    }
}
