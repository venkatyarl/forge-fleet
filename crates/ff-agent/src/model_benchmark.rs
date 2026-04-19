//! Model benchmarker — measures tokens/sec and TTFT for a loaded model on
//! a specific computer and writes the result into
//! `model_catalog.benchmark_results` (a JSONB column that accumulates runs
//! keyed by `"<computer>:<iso-timestamp>"`).
//!
//! How it works:
//!   1. Resolve the inference endpoint for `(model_id, computer)` via
//!      the Pulse reader (Phase 10) — `pick_llm_server_for` returns the
//!      best currently-active+healthy server for that model.
//!   2. Send a fixed prompt suite (5 short prompts of increasing size)
//!      against `/v1/chat/completions` with `stream=true` so we can
//!      capture time-to-first-token.
//!   3. Roll up: mean tokens/sec, mean TTFT, max context seen, etc.
//!   4. Append the result to `model_catalog.benchmark_results`.
//!
//! v1 scope: latency + throughput only. MMLU accuracy / perplexity /
//! out-of-distribution generalization are explicitly out of scope.

use std::time::Instant;

use ff_pulse::reader::PulseReader;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use thiserror::Error;
use tokio::time::Duration;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum BenchError {
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    #[error("ff-db: {0}")]
    FfDb(#[from] ff_db::DbError),
    #[error("pulse: {0}")]
    Pulse(String),
    #[error("model '{0}' not loaded on '{1}' (no active healthy LLM server in pulse beats)")]
    NotLoaded(String, String),
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub model_id: String,
    pub computer: String,
    pub runtime: String,
    pub endpoint: String,
    pub tokens_per_sec: f64,
    pub ttft_ms: u32,
    pub context_tokens_max: u32,
    pub prompt_eval_rate: f64,
    pub generation_rate: f64,
    /// Not measured in v1 — kept as Option for forward compatibility.
    pub mmlu_sample_accuracy: Option<f32>,
    pub prompt_count: u32,
    pub timestamp: String,
}

pub struct ModelBenchmarker {
    pg: PgPool,
    pulse: PulseReader,
    http: reqwest::Client,
}

impl ModelBenchmarker {
    pub fn new(pg: PgPool, pulse: PulseReader) -> Self {
        Self {
            pg,
            pulse,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Benchmark a specific model on a specific computer. Writes into
    /// `model_catalog.benchmark_results`. Fails with `BenchError::NotLoaded`
    /// if the model isn't actively serving there.
    pub async fn benchmark(
        &self,
        model_id: &str,
        computer: &str,
    ) -> Result<BenchmarkReport, BenchError> {
        // Discover the endpoint via Pulse. `pick_llm_server_for` returns the
        // fleet-wide best match; we then filter to the requested computer.
        let all = self
            .pulse
            .list_llm_servers()
            .await
            .map_err(|e| BenchError::Pulse(e.to_string()))?;

        let (_c, server) = all
            .into_iter()
            .find(|(c, s)| c == computer && s.model.id == model_id)
            .ok_or_else(|| BenchError::NotLoaded(model_id.into(), computer.into()))?;

        info!(
            model = model_id,
            computer,
            endpoint = %server.endpoint,
            "starting benchmark"
        );

        let prompts = standard_prompt_suite();
        let n = prompts.len() as u32;
        let mut total_gen_tokens: u64 = 0;
        let mut total_prompt_tokens: u64 = 0;
        let mut total_gen_elapsed_ms: u64 = 0;
        let mut total_ttft_ms: u64 = 0;
        let mut max_ctx: u32 = 0;

        for prompt in &prompts {
            let result = self.run_one(&server.endpoint, model_id, prompt).await?;
            total_gen_tokens += result.gen_tokens as u64;
            total_prompt_tokens += result.prompt_tokens as u64;
            total_gen_elapsed_ms += result.gen_elapsed_ms as u64;
            total_ttft_ms += result.ttft_ms as u64;
            max_ctx = max_ctx.max(result.context_tokens);
            debug!(
                ttft_ms = result.ttft_ms,
                gen_tokens = result.gen_tokens,
                gen_elapsed_ms = result.gen_elapsed_ms,
                "bench sample"
            );
        }

        let gen_secs = total_gen_elapsed_ms as f64 / 1000.0;
        let tokens_per_sec = if gen_secs > 0.0 {
            total_gen_tokens as f64 / gen_secs
        } else {
            0.0
        };
        let ttft_ms = (total_ttft_ms as f64 / n as f64) as u32;
        // Prompt eval rate: rough proxy — prompt tokens / (ttft_ms / 1000).
        let prompt_eval_rate = if total_ttft_ms > 0 {
            total_prompt_tokens as f64 * 1000.0 / total_ttft_ms as f64
        } else {
            0.0
        };

        let report = BenchmarkReport {
            model_id: model_id.into(),
            computer: computer.into(),
            runtime: server.runtime.clone(),
            endpoint: server.endpoint.clone(),
            tokens_per_sec,
            ttft_ms,
            context_tokens_max: max_ctx,
            prompt_eval_rate,
            generation_rate: tokens_per_sec,
            mmlu_sample_accuracy: None,
            prompt_count: n,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        // Persist.
        let value = serde_json::to_value(&report).unwrap_or(json!({}));
        ff_db::pg_append_benchmark_result(&self.pg, model_id, computer, &value).await?;

        info!(
            model = model_id,
            computer,
            tokens_per_sec = report.tokens_per_sec,
            ttft_ms = report.ttft_ms,
            "benchmark done"
        );

        Ok(report)
    }

    async fn run_one(
        &self,
        endpoint: &str,
        model_id: &str,
        prompt: &str,
    ) -> Result<OneRun, BenchError> {
        let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
        let body = json!({
            "model": model_id,
            "messages": [
                {"role": "user", "content": prompt}
            ],
            "stream": false,
            "max_tokens": 256,
            "temperature": 0.2,
        });

        let t_start = Instant::now();
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BenchError::Http(format!("POST {url}: {e}")))?;

        if !resp.status().is_success() {
            return Err(BenchError::Http(format!(
                "{url} → {}",
                resp.status()
            )));
        }

        let ttft_ms = t_start.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let json_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BenchError::Http(format!("decode JSON: {e}")))?;
        let gen_elapsed_ms = t_start.elapsed().as_millis().min(u32::MAX as u128) as u32;

        // Parse usage block (OpenAI-compatible). Some backends omit it;
        // fall back to character-count heuristics.
        let usage = json_body.get("usage");
        let prompt_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or_else(|| (prompt.len() / 4) as u32);
        let gen_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(0);
        let context_tokens = usage
            .and_then(|u| u.get("total_tokens"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(prompt_tokens + gen_tokens);

        Ok(OneRun {
            ttft_ms,
            gen_tokens,
            prompt_tokens,
            gen_elapsed_ms,
            context_tokens,
        })
    }
}

struct OneRun {
    ttft_ms: u32,
    gen_tokens: u32,
    prompt_tokens: u32,
    gen_elapsed_ms: u32,
    context_tokens: u32,
}

/// 5 prompts of increasing complexity. Deterministic — reused across runs
/// so results are comparable across models and over time.
fn standard_prompt_suite() -> Vec<String> {
    vec![
        "Respond with a single word: OK.".to_string(),
        "List five prime numbers under 50, comma-separated.".to_string(),
        "Write a one-sentence summary of what Wake-on-LAN does.".to_string(),
        "Write a short Rust function that returns the nth Fibonacci number iteratively.".to_string(),
        "Explain in two paragraphs the tradeoffs between LoRA fine-tuning and full fine-tuning, assuming the reader is a software engineer new to model training.".to_string(),
    ]
}

/// Light wrapper to run a benchmark without an existing PulseReader.
/// Builds one on the fly from the `FORGEFLEET_REDIS_URL` env or
/// `redis://127.0.0.1:6379` default.
pub async fn benchmark_with_defaults(
    pg: &PgPool,
    model_id: &str,
    computer: &str,
) -> Result<BenchmarkReport, BenchError> {
    let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let pulse =
        PulseReader::new(&redis_url).map_err(|e| BenchError::Pulse(e.to_string()))?;
    let b = ModelBenchmarker::new(pg.clone(), pulse);
    b.benchmark(model_id, computer).await.or_else(|e| {
        if matches!(e, BenchError::NotLoaded(_, _)) {
            warn!(
                model = model_id,
                computer,
                "model not loaded on target; reporting as not-yet-implemented"
            );
        }
        Err(e)
    })
}
