//! Local MLX embedding client (stub).
//!
//! When the local embedding server is deployed, this client will send
//! text to it and receive float vectors back. Until then, it returns
//! zero vectors as placeholders.

/// Client for a local OpenAI-compatible embedding endpoint.
pub struct EmbeddingClient {
    pub endpoint: String,
    pub model_id: String,
}

impl EmbeddingClient {
    pub fn new(endpoint: &str, model_id: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            model_id: model_id.to_string(),
        }
    }

    /// Embed a single text. Returns vector of f32.
    ///
    /// Currently returns a dummy 384-dim zero vector since the embedding
    /// server isn't deployed yet. When the server is up, this will POST
    /// to the endpoint and parse the real vector.
    pub async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
        // TODO: Once the embedding server is running at self.endpoint,
        // replace this with an actual HTTP call:
        //
        // let resp = reqwest::Client::new()
        //     .post(format!("{}/v1/embeddings", self.endpoint))
        //     .json(&serde_json::json!({
        //         "model": self.model_id,
        //         "input": text,
        //     }))
        //     .send().await.map_err(|e| e.to_string())?;
        //
        // Then parse the response.

        Ok(vec![0.0f32; 384])
    }

    /// Embed a batch of texts.
    ///
    /// Currently returns dummy zero vectors for each input.
    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| vec![0.0f32; 384]).collect())
    }
}
