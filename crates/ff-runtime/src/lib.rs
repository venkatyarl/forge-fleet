//! `ff-runtime` — ForgeFleet inference engine management.
//!
//! This crate provides a unified interface for managing different inference
//! server runtimes across the fleet:
//!
//! - **llama.cpp** — Primary for Mac (Metal), AMD (Vulkan), CPU, and NVIDIA (CUDA)
//! - **vLLM** — High-throughput serving on NVIDIA GPUs / DGX Spark
//! - **MLX** — Apple Silicon native via Apple's MLX framework
//! - **Ollama** — Easy-setup fallback that works everywhere
//!
//! # Architecture
//!
//! All engines implement the [`InferenceEngine`] trait, which provides:
//! - `start()` / `stop()` — lifecycle management via `std::process::Command`
//! - `health_check()` — HTTP health probes
//! - `get_models()` — query loaded models via OpenAI-compatible API
//! - `get_endpoint()` — get the API endpoint URL
//!
//! The [`detector`] module auto-selects the best runtime for the current hardware,
//! and [`model_manager`] handles downloading and tracking model files.
//!
//! # Example
//!
//! ```rust,no_run
//! use ff_runtime::detector;
//! use ff_runtime::engine::EngineConfig;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let hw = ff_core::hardware::detect()?;
//! let mut engine = detector::create_engine(&hw, None);
//!
//! let config = EngineConfig {
//!     model_path: "/models/qwen3-32b-q4.gguf".into(),
//!     model_id: "qwen3-32b".into(),
//!     port: 51800,
//!     ..Default::default()
//! };
//!
//! let pid = engine.start(&config).await?;
//! println!("Engine running on PID {pid}");
//! # Ok(())
//! # }
//! ```

pub mod detector;
pub mod engine;
pub mod error;
pub mod llamacpp;
pub mod mlx;
pub mod model_manager;
pub mod ollama;
pub mod process_manager;
pub mod vllm;

// Re-export key types at crate root.
pub use engine::{EngineConfig, EngineStatus, InferenceEngine};
pub use error::{Result, RuntimeError};
pub use process_manager::{DetectedProcess, ManagedModel, ProcessManager, ProcessManagerConfig};
