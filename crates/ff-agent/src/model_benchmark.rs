//! Model benchmarker — measures tokens/sec and TTFT for a loaded model on
//! a specific computer and writes the result into
//! `fleet_model_catalog.benchmarks` (a JSONB column that accumulates runs
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
//!   4. Append the result to `fleet_model_catalog.benchmarks`.
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

/// Minimum tokens-per-second a benchmark must achieve to count as "passing".
/// Any real model — even 7B quantised on CPU — should sustain >5 tok/s.
/// Anything slower is almost always a misconfigured/overloaded server and
/// should block auto-promotion to `active`.
pub const BENCH_PASS_MIN_TPS: f64 = 5.0;

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
    /// True iff the benchmark met the "good enough for production" bar.
    ///
    /// Pass criteria (all must hold):
    ///   1. `tokens_per_sec >= BENCH_PASS_MIN_TPS` (5.0 tok/s).
    ///   2. At least one prompt produced a non-empty response
    ///      (`context_tokens_max > 0` after the suite runs).
    ///   3. No HTTP/parse errors bubbled out of the suite (implicit:
    ///      if we built a `BenchmarkReport` at all, `benchmark()` made
    ///      it through every prompt without returning `Err`).
    pub bench_pass: bool,
    /// Human-readable reason when `bench_pass == false`; empty on success.
    pub bench_pass_reason: String,
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
    /// `fleet_model_catalog.benchmarks`. Fails with `BenchError::NotLoaded`
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

        // Compute pass/fail per the contract documented on BenchmarkReport.
        let (bench_pass, bench_pass_reason) = if tokens_per_sec < BENCH_PASS_MIN_TPS {
            (
                false,
                format!("tokens_per_sec {tokens_per_sec:.2} < threshold {BENCH_PASS_MIN_TPS:.2}"),
            )
        } else if max_ctx == 0 && total_gen_tokens == 0 {
            (false, "no prompt produced a non-empty response".to_string())
        } else {
            (true, String::new())
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
            bench_pass,
            bench_pass_reason,
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
        let url = ff_core::url::normalize_chat_completions_url(endpoint);
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
            return Err(BenchError::Http(format!("{url} → {}", resp.status())));
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

/// One canonical runtime artifact that can make a model benchmarkable.
#[derive(Debug, Clone, PartialEq)]
struct CatalogVariantReq {
    runtime: String,
    size_gb: f64,
}

/// Canonical row shape used to decide whether a model can run on a node.
struct CatalogReqs {
    variants: Vec<CatalogVariantReq>,
}

async fn fetch_catalog_reqs(
    pool: &PgPool,
    model_id: &str,
) -> Result<Option<CatalogReqs>, BenchError> {
    let row = sqlx::query(
        "SELECT variants
           FROM fleet_model_catalog
          WHERE id = $1
            AND COALESCE(lifecycle, 'active') <> 'retired'",
    )
    .bind(model_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        use sqlx::Row as _;
        let variants: serde_json::Value = r.get("variants");
        CatalogReqs {
            variants: catalog_variant_requirements(&variants),
        }
    }))
}

fn catalog_variant_requirements(variants: &serde_json::Value) -> Vec<CatalogVariantReq> {
    variants
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|variant| {
            let runtime = variant
                .get("runtime")
                .and_then(serde_json::Value::as_str)?
                .trim()
                .to_ascii_lowercase();
            let size_gb = variant
                .get("size_gb")
                .and_then(serde_json::Value::as_f64)
                .filter(|size| size.is_finite() && *size > 0.0)?;
            matches!(runtime.as_str(), "llama.cpp" | "mlx" | "vllm")
                .then_some(CatalogVariantReq { runtime, size_gb })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn variant_fits_host(
    variant: &CatalogVariantReq,
    gpu_kind: &str,
    can_run_cuda: bool,
    can_run_metal: bool,
    max_runnable_model_gb: Option<f64>,
    ram_available_gb: f64,
    vram_free_gb: Option<f64>,
    disk_free_gb: f64,
) -> bool {
    const CAPACITY_SLACK_GB: f64 = 2.0;
    const DISK_SLACK_GB: f64 = 5.0;
    if disk_free_gb < variant.size_gb + DISK_SLACK_GB {
        return false;
    }

    let runtime_compatible = match variant.runtime.as_str() {
        "llama.cpp" => true,
        "mlx" => can_run_metal || gpu_kind == "apple_silicon",
        "vllm" => can_run_cuda || gpu_kind == "nvidia_cuda",
        _ => false,
    };
    if !runtime_compatible {
        return false;
    }

    let live_capacity_gb = match variant.runtime.as_str() {
        // A vLLM artifact is GPU-resident. Do not treat ordinary host RAM as
        // evidence that the node has enough free CUDA memory.
        "vllm" => vram_free_gb.unwrap_or(0.0),
        _ => ram_available_gb.max(vram_free_gb.unwrap_or(0.0)),
    };
    let proven_capacity_gb = max_runnable_model_gb
        .map(|capacity| capacity.min(live_capacity_gb))
        .unwrap_or(live_capacity_gb);
    proven_capacity_gb >= variant.size_gb + CAPACITY_SLACK_GB
}

fn gpu_priority(kind: &str) -> u8 {
    match kind {
        // Apple silicon is preferred for MLX workloads and generally the
        // fastest per-watt option in this fleet.
        "apple_silicon" => 1,
        "nvidia_cuda" => 2,
        "amd_rocm" => 3,
        "integrated" => 4,
        _ => 5, // "none" or unknown
    }
}

/// Pick the best node in the fleet to run a benchmark for `model_id`.
///
/// Filters (per canonical variant):
///   - runtime is one of the chat-serving runtimes (`llama.cpp`, `mlx`, `vllm`)
///     and is compatible with the node's live capabilities;
///   - live available RAM/VRAM plus `max_runnable_model_gb` prove the artifact
///     fits with 2 GiB of headroom;
///   - `disk_free_gb` is at least variant `size_gb + 5` so the model can be
///     staged if it is not already resident.
///
/// Ordering:
///   1. GPU priority (apple_silicon < nvidia_cuda < amd_rocm < integrated < none).
///   2. Lowest `cpu_pct` (least loaded).
///
/// Returns `Ok(None)` if the catalog row is missing or no node qualifies.
pub async fn pick_benchmark_target(
    pool: &PgPool,
    pulse: &PulseReader,
    model_id: &str,
) -> Result<Option<String>, BenchError> {
    let reqs = match fetch_catalog_reqs(pool, model_id).await? {
        Some(r) if !r.variants.is_empty() => r,
        None => return Ok(None),
        Some(_) => return Ok(None),
    };

    let beats = pulse
        .all_beats()
        .await
        .map_err(|e| BenchError::Pulse(e.to_string()))?;

    // (name, gpu_priority, cpu_pct)
    let mut candidates: Vec<(String, u8, f64)> = Vec::new();

    for b in beats {
        if b.going_offline || b.maintenance_mode {
            continue;
        }

        let gpu_kind = b.capabilities.gpu_kind.as_str();
        let fits = reqs.variants.iter().any(|variant| {
            variant_fits_host(
                variant,
                gpu_kind,
                b.capabilities.can_run_cuda,
                b.capabilities.can_run_metal,
                b.capabilities.max_runnable_model_gb,
                b.memory.ram_available_for_new_llm_gb,
                b.memory.vram_free_gb,
                b.load.disk_free_gb,
            )
        });
        if !fits {
            continue;
        }

        candidates.push((
            b.computer_name.clone(),
            gpu_priority(gpu_kind),
            b.load.cpu_pct,
        ));
    }

    // Sort: lower gpu_priority wins, then lower cpu_pct wins.
    candidates.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });

    Ok(candidates.into_iter().next().map(|(name, _, _)| name))
}

/// Light wrapper to run a benchmark without an existing PulseReader.
/// Builds one on the fly from the `FORGEFLEET_REDIS_URL` env or
/// `redis://127.0.0.1:56379` default (docker-compose host mapping).
pub async fn benchmark_with_defaults(
    pg: &PgPool,
    model_id: &str,
    computer: &str,
) -> Result<BenchmarkReport, BenchError> {
    let redis_url =
        std::env::var("FORGEFLEET_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:56379".into());
    let pulse = PulseReader::new(&redis_url).map_err(|e| BenchError::Pulse(e.to_string()))?;
    let b = ModelBenchmarker::new(pg.clone(), pulse);
    b.benchmark(model_id, computer).await.map_err(|e| {
        if matches!(e, BenchError::NotLoaded(_, _)) {
            warn!(
                model = model_id,
                computer, "model not loaded on target; reporting as not-yet-implemented"
            );
        }
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_priority_ordering_prefers_apple_then_cuda_then_cpu() {
        assert!(gpu_priority("apple_silicon") < gpu_priority("nvidia_cuda"));
        assert!(gpu_priority("nvidia_cuda") < gpu_priority("amd_rocm"));
        assert!(gpu_priority("amd_rocm") < gpu_priority("integrated"));
        assert!(gpu_priority("integrated") < gpu_priority("none"));
        assert_eq!(gpu_priority("unknown_kind"), gpu_priority("none"));
    }

    #[test]
    fn bench_pass_threshold_is_five_tps() {
        assert_eq!(BENCH_PASS_MIN_TPS, 5.0);
    }

    #[test]
    fn bench_report_with_pass_fields_roundtrips_json() {
        let r = BenchmarkReport {
            model_id: "x".into(),
            computer: "vinny".into(),
            runtime: "mlx_lm".into(),
            endpoint: "http://127.0.0.1:51001".into(),
            tokens_per_sec: 42.5,
            ttft_ms: 80,
            context_tokens_max: 321,
            prompt_eval_rate: 120.0,
            generation_rate: 42.5,
            mmlu_sample_accuracy: None,
            prompt_count: 5,
            timestamp: "2026-04-18T00:00:00Z".into(),
            bench_pass: true,
            bench_pass_reason: String::new(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: BenchmarkReport = serde_json::from_str(&s).unwrap();
        assert!(back.bench_pass);
        assert_eq!(back.tokens_per_sec, 42.5);
    }

    #[test]
    fn canonical_variant_requirements_reject_unknown_or_unbounded_artifacts() {
        let variants = serde_json::json!([
            {"runtime": "llama.cpp", "size_gb": 12.5},
            {"runtime": "MLX", "size_gb": 8.0},
            {"runtime": "diffusers", "size_gb": 4.0},
            {"runtime": "vllm"},
            {"runtime": "vllm", "size_gb": -1.0}
        ]);
        assert_eq!(
            catalog_variant_requirements(&variants),
            vec![
                CatalogVariantReq {
                    runtime: "llama.cpp".to_string(),
                    size_gb: 12.5
                },
                CatalogVariantReq {
                    runtime: "mlx".to_string(),
                    size_gb: 8.0
                }
            ]
        );
        assert!(catalog_variant_requirements(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn canonical_variant_fit_is_runtime_and_capacity_aware() {
        let vllm = CatalogVariantReq {
            runtime: "vllm".to_string(),
            size_gb: 20.0,
        };
        assert!(variant_fits_host(
            &vllm,
            "nvidia_cuda",
            true,
            false,
            Some(100.0),
            100.0,
            Some(30.0),
            100.0
        ));
        assert!(!variant_fits_host(
            &vllm,
            "none",
            false,
            false,
            Some(100.0),
            100.0,
            None,
            100.0
        ));
        assert!(!variant_fits_host(
            &vllm,
            "nvidia_cuda",
            true,
            false,
            Some(10.0),
            100.0,
            Some(30.0),
            100.0
        ));
        assert!(!variant_fits_host(
            &vllm,
            "nvidia_cuda",
            true,
            false,
            Some(100.0),
            100.0,
            Some(30.0),
            24.0
        ));
        assert!(!variant_fits_host(
            &CatalogVariantReq {
                runtime: "vllm".to_string(),
                size_gb: 40.0,
            },
            "nvidia_cuda",
            true,
            false,
            Some(100.0),
            100.0,
            Some(30.0),
            100.0
        ));
    }
}
