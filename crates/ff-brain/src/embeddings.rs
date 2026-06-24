//! Local OpenAI-compatible embedding client.
//!
//! Posts text to a local embedding endpoint (e.g. mlx_lm.server,
//! ollama, or llama.cpp) and receives float vectors back.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use sqlx::PgPool;

async fn pick_fleet_workload_endpoint(pool: &PgPool, workload: &str) -> Option<(String, String)> {
    let filter = ff_db::RouteFilter {
        workload: Some(workload.to_string()),
        limit: 1,
        ..Default::default()
    };
    let route = ff_db::pg_route_deployments(pool, &filter)
        .await
        .ok()?
        .into_iter()
        .next()?;
    let model = route
        .catalog_id
        .or(route.catalog_name)
        .unwrap_or_else(|| workload.to_string());
    Some((route.endpoint, model))
}

/// Resolution order for picking an embedding endpoint:
///
///   1. `FF_EMBEDDING_ENDPOINT` + `FF_EMBEDDING_MODEL` env vars (operator override)
///   2. A healthy `fleet_model_deployments` row whose catalog has
///      `preferred_workloads` containing "embedding" or "embeddings"
///   3. Hash stub (deterministic 384-dim noise — only useful for tests)
///
/// Step (2) is what makes `ff brain search` actually search semantically
/// once a `bge-m3` / `qwen3-embedding-8b` / similar is loaded somewhere
/// on the fleet. Before this resolver existed, the brain was searching
/// against random hash vectors whenever the env vars weren't set.
async fn pick_fleet_embedding_endpoint(pool: &PgPool) -> Option<(String, String)> {
    pick_fleet_workload_endpoint(pool, "embedding").await
}

/// Discover a live fleet embedding endpoint and return a ready
/// [`EmbeddingClient`]. Honours the same `FF_EMBEDDING_ENDPOINT` /
/// `FF_EMBEDDING_MODEL` override as the generators, then falls back to a
/// healthy fleet deployment. Returns `None` when no real endpoint exists —
/// callers that must avoid the hash stub (e.g. Cortex bulk embedding, which
/// would otherwise persist garbage vectors) abort on `None` rather than store
/// noise.
pub async fn fleet_embedding_client(pool: &PgPool) -> Option<EmbeddingClient> {
    if let (Ok(endpoint), Ok(model)) = (
        std::env::var("FF_EMBEDDING_ENDPOINT"),
        std::env::var("FF_EMBEDDING_MODEL"),
    ) {
        return Some(EmbeddingClient::new(&endpoint, &model));
    }
    let (endpoint, model) = pick_fleet_embedding_endpoint(pool).await?;
    Some(EmbeddingClient::new(&endpoint, &model))
}

async fn pick_fleet_rerank_endpoint(pool: &PgPool) -> Option<String> {
    if let Ok(endpoint) = std::env::var("FF_RERANK_ENDPOINT") {
        return Some(endpoint);
    }
    let (endpoint, _) = pick_fleet_workload_endpoint(pool, "reranking").await?;
    Some(endpoint)
}

/// Rerank `documents` against `query` using a fleet reranker deployment.
///
/// The reranker serves llama.cpp's OpenAI-style `/v1/rerank` endpoint
/// (`llama-server --reranking`). Returns original document indexes plus
/// relevance scores, sorted descending and capped to `top_n`. If no healthy
/// reranking endpoint can be routed, this returns `Err` so callers can preserve
/// their cheaper first-stage order.
pub async fn fleet_rerank(
    pool: &PgPool,
    query: &str,
    documents: &[String],
    top_n: usize,
) -> Result<Vec<(usize, f32)>, String> {
    if documents.is_empty() || top_n == 0 {
        return Ok(Vec::new());
    }
    let endpoint = pick_fleet_rerank_endpoint(pool)
        .await
        .ok_or_else(|| "no healthy fleet reranking endpoint".to_string())?;
    let resp = EMBEDDING_HTTP_CLIENT
        .post(format!("{endpoint}/v1/rerank"))
        .json(&serde_json::json!({
            "query": query,
            "documents": documents,
            "top_n": top_n,
        }))
        .send()
        .await
        .map_err(|e| format!("rerank request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("rerank server returned {status}: {body}"));
    }

    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("rerank response parse failed: {e}"))?;
    let results = payload
        .get("results")
        .or_else(|| payload.get("data"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "rerank response missing results array".to_string())?;

    let mut out = Vec::with_capacity(results.len());
    for item in results {
        let index = item
            .get("index")
            .or_else(|| item.get("document_index"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "rerank result missing index".to_string())? as usize;
        if index >= documents.len() {
            return Err(format!("rerank result index {index} out of bounds"));
        }
        let score = item
            .get("relevance_score")
            .or_else(|| item.get("score"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| "rerank result missing relevance_score".to_string())?
            as f32;
        out.push((index, score));
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(top_n.min(documents.len()));
    Ok(out)
}

/// Generate an embedding for `text`, using only env vars / hash fallback.
///
/// This is the no-pool variant kept for backwards compatibility. Callers
/// that have a `PgPool` should prefer [`generate_embedding_with_pool`] so
/// the fleet's actual embedding deployment can be auto-discovered.
pub async fn generate_embedding(text: &str) -> Vec<f32> {
    if let (Ok(endpoint), Ok(model)) = (
        std::env::var("FF_EMBEDDING_ENDPOINT"),
        std::env::var("FF_EMBEDDING_MODEL"),
    ) {
        let client = EmbeddingClient::new(&endpoint, &model);
        match client.embed(text).await {
            Ok(vec) => return vec,
            Err(e) => {
                tracing::warn!("embedding server failed, falling back to hash stub: {e}");
            }
        }
    }
    hash_fallback(text)
}

/// Generate an embedding for `text`, auto-discovering a fleet endpoint
/// from `fleet_model_deployments` when no env override is set.
///
/// Resolution: env vars → fleet auto-discovery → hash fallback.
pub async fn generate_embedding_with_pool(text: &str, pool: &PgPool) -> Vec<f32> {
    // (1) env override
    if let (Ok(endpoint), Ok(model)) = (
        std::env::var("FF_EMBEDDING_ENDPOINT"),
        std::env::var("FF_EMBEDDING_MODEL"),
    ) {
        let client = EmbeddingClient::new(&endpoint, &model);
        match client.embed(text).await {
            Ok(vec) => return vec,
            Err(e) => {
                tracing::warn!("env-configured embedding server failed: {e}");
            }
        }
    }

    // (2) fleet auto-discovery
    if let Some((endpoint, model)) = pick_fleet_embedding_endpoint(pool).await {
        let client = EmbeddingClient::new(&endpoint, &model);
        match client.embed(text).await {
            Ok(vec) => return vec,
            Err(e) => {
                tracing::warn!(
                    endpoint = %endpoint,
                    model    = %model,
                    "fleet-discovered embedding endpoint failed; falling back to hash stub: {e}"
                );
            }
        }
    } else {
        tracing::warn!(
            "no healthy embedding deployment in fleet_model_deployments; \
             vault search is running on hash-stub vectors. Load one with: \
             ff model autoload bge-m3"
        );
    }

    // (3) hash stub — last resort
    hash_fallback(text)
}

/// Deterministic 384-dim noise vector — only useful for tests / infra dev
/// where no real embedder is available.
fn hash_fallback(text: &str) -> Vec<f32> {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let seed = hasher.finish();

    let mut vec = Vec::with_capacity(384);
    for i in 0..384 {
        let mut h = DefaultHasher::new();
        (seed, i).hash(&mut h);
        let val = (h.finish() as f32 / u64::MAX as f32) * 2.0 - 1.0;
        vec.push(val);
    }
    vec
}

/// Client for a local OpenAI-compatible embedding endpoint.
pub struct EmbeddingClient {
    pub endpoint: String,
    pub model_id: String,
    pub dimensions: usize,
    client: reqwest::Client,
}

/// Shared reqwest client for every `EmbeddingClient`. Each instance used to
/// build its own (with DNS resolver + TLS state); under heavy brain traffic
/// that was per-message churn. `reqwest::Client` is internally `Arc<Inner>`
/// so cloning the shared one is cheap.
static EMBEDDING_HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("build shared embedding reqwest client")
    });

impl EmbeddingClient {
    pub fn new(endpoint: &str, model_id: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            model_id: model_id.to_string(),
            dimensions: 384,
            client: EMBEDDING_HTTP_CLIENT.clone(),
        }
    }

    /// Embed a single text. Returns vector of f32.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        let resp = self
            .client
            .post(format!("{}/v1/embeddings", self.endpoint))
            .json(&serde_json::json!({
                "model": self.model_id,
                "input": text,
            }))
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

        let embedding = payload
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|arr| arr.first())
            .and_then(|entry| entry.get("embedding"))
            .and_then(|emb| emb.as_array())
            .ok_or_else(|| "embedding response missing data[0].embedding".to_string())?;

        let vec: Vec<f32> = embedding
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        if vec.is_empty() {
            return Err("embedding vector is empty".to_string());
        }

        Ok(vec)
    }

    /// Embed a batch of texts.
    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let resp = self
            .client
            .post(format!("{}/v1/embeddings", self.endpoint))
            .json(&serde_json::json!({
                "model": self.model_id,
                "input": texts,
            }))
            .send()
            .await
            .map_err(|e| format!("embedding batch request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("embedding server returned {status}: {body}"));
        }

        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("embedding batch response parse failed: {e}"))?;

        let data = payload
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| "embedding batch response missing data array".to_string())?;

        let mut results = Vec::with_capacity(texts.len());
        for entry in data {
            let vec = entry
                .get("embedding")
                .and_then(|emb| emb.as_array())
                .ok_or_else(|| "embedding entry missing embedding array".to_string())?
                .iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect();
            results.push(vec);
        }

        Ok(results)
    }
}
