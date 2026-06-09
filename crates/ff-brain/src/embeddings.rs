//! Local OpenAI-compatible embedding client.
//!
//! Posts text to a local embedding endpoint (e.g. mlx_lm.server,
//! ollama, or llama.cpp) and receives float vectors back.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use sqlx::{PgPool, Row};

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
    // Join deployments → catalog → workers; prefer healthy rows; cheapest tier
    // first so a 568M bge-m3 wins over an 8B qwen3-embedding if both run.
    //
    // computers.primary_ip is the source of truth for the worker's LAN IP
    // (fleet_workers no longer carries an ip column post-V83). We LEFT JOIN
    // computers as a fallback for pre-V83 deployments still keyed by name.
    let row = sqlx::query(
        r#"
        SELECT d.port,
               COALESCE(c.primary_ip, w.name) AS host_or_name,
               cat.id AS catalog_id,
               cat.tier AS tier
        FROM fleet_model_deployments d
        JOIN fleet_model_catalog cat ON cat.id = d.catalog_id
        LEFT JOIN fleet_workers w     ON w.name = d.worker_name
        LEFT JOIN computers c         ON LOWER(c.name) = LOWER(d.worker_name)
        WHERE d.health_status = 'healthy'
          AND (cat.preferred_workloads @> '["embedding"]'::jsonb
            OR cat.preferred_workloads @> '["embeddings"]'::jsonb)
        ORDER BY cat.tier ASC, d.last_health_at DESC NULLS LAST
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;

    let port: i32 = row.try_get("port").ok()?;
    let host: String = row.try_get("host_or_name").ok()?;
    let catalog_id: String = row.try_get("catalog_id").ok()?;
    let endpoint = format!("http://{host}:{port}");
    Some((endpoint, catalog_id))
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
