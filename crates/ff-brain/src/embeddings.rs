//! Local OpenAI-compatible embedding client.
//!
//! Posts text to a local embedding endpoint (e.g. mlx_lm.server,
//! ollama, or llama.cpp) and receives float vectors back.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Generate a deterministic fake embedding for testing / infrastructure.
/// Produces a 384-dimensional vector derived from the hash of `text`.
///
/// TODO: Replace with real embedding model (e.g. all-MiniLM-L6-v2 via ONNX)
pub fn generate_embedding(text: &str) -> Vec<f32> {
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
}

impl EmbeddingClient {
    pub fn new(endpoint: &str, model_id: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            model_id: model_id.to_string(),
            dimensions: 384,
        }
    }

    /// Embed a single text. Returns vector of f32.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        let resp = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap_or_else(|_| reqwest::Client::new())
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

        let resp = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap_or_else(|_| reqwest::Client::new())
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
