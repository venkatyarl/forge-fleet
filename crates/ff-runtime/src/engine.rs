//! `InferenceEngine` — the trait every runtime backend implements.
//!
//! Each engine manages one inference server process (llama-server, vllm serve,
//! mlx_lm.server, ollama serve) and exposes a uniform interface.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::error::Result;

// ─── Engine Configuration ────────────────────────────────────────────────────

/// Configuration for starting an inference engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Path to the model file (GGUF for llama.cpp, safetensors dir for vLLM).
    pub model_path: PathBuf,
    /// Model identifier (e.g. "qwen3-32b-q4").
    pub model_id: String,
    /// Host to bind the server to.
    pub host: String,
    /// Port to bind the server to.
    pub port: u16,
    /// Context window size.
    pub ctx_size: u32,
    /// Number of GPU layers to offload (-1 = all).
    pub gpu_layers: i32,
    /// Number of parallel request slots.
    pub parallel: u32,
    /// Additional engine-specific arguments.
    pub extra_args: Vec<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model_path: PathBuf::new(),
            model_id: String::new(),
            host: "0.0.0.0".into(),
            port: 51800,
            ctx_size: 8192,
            gpu_layers: -1,
            parallel: 4,
            extra_args: Vec::new(),
        }
    }
}

// ─── Engine Status ───────────────────────────────────────────────────────────

/// Current status of a running inference engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    /// Whether the engine process is running.
    pub running: bool,
    /// Whether the health endpoint responds OK.
    pub healthy: bool,
    /// PID of the engine process (if running).
    pub pid: Option<u32>,
    /// Model currently loaded.
    pub model_id: Option<String>,
    /// Endpoint URL (e.g. "http://0.0.0.0:51800").
    pub endpoint: Option<String>,
    /// Uptime in seconds.
    pub uptime_secs: Option<u64>,
}

// ─── Boxed future alias ──────────────────────────────────────────────────────

/// A boxed future that is Send.
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ─── InferenceEngine trait ───────────────────────────────────────────────────

/// Trait that all inference engine backends must implement.
///
/// Uses explicit `BoxFut` return types instead of `async fn` so the trait
/// is dyn-compatible (object-safe).
pub trait InferenceEngine: Send + Sync {
    /// Human-readable name of this engine (e.g. "llama.cpp", "vLLM").
    fn name(&self) -> &str;

    /// Start the inference server with the given configuration.
    ///
    /// Returns the PID of the spawned process.
    fn start(&mut self, config: &EngineConfig) -> BoxFut<'_, Result<u32>>;

    /// Stop the running inference server gracefully.
    ///
    /// Falls back to SIGKILL if graceful stop times out.
    fn stop(&mut self) -> BoxFut<'_, Result<()>>;

    /// Check if the engine is healthy (responds to /health or equivalent).
    fn health_check(&self) -> BoxFut<'_, Result<bool>>;

    /// Get the list of models currently loaded.
    fn get_models(&self) -> BoxFut<'_, Result<Vec<String>>>;

    /// Get the full endpoint URL for API access.
    fn get_endpoint(&self) -> Option<String>;

    /// Get current engine status.
    fn status(&self) -> BoxFut<'_, EngineStatus>;
}
