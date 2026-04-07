//! Runtime auto-detection.
//!
//! Determines the best inference runtime for the current hardware:
//! - **Mac (Apple Silicon)** → llama.cpp (Metal) or MLX
//! - **Ubuntu AMD (no discrete GPU)** → llama.cpp (CPU)
//! - **AMD RDNA iGPU** → llama.cpp (Vulkan)
//! - **DGX Spark (single)** → vLLM or llama.cpp (CUDA)
//! - **DGX Spark (linked pair)** → vLLM (tensor parallel)

use tracing::info;

use ff_core::types::{GpuType, Hardware, OsType, Runtime};

use crate::engine::InferenceEngine;
use crate::llamacpp::{LlamaCppBackend, LlamaCppEngine};
use crate::mlx::MlxEngine;
use crate::ollama::OllamaEngine;
use crate::vllm::{VllmConfig, VllmEngine};

// ─── Detection Result ────────────────────────────────────────────────────────

/// The recommended runtime configuration for the detected hardware.
#[derive(Debug)]
pub struct RuntimeRecommendation {
    /// Primary recommended runtime.
    pub primary: Runtime,
    /// Alternative runtimes that could also work.
    pub alternatives: Vec<Runtime>,
    /// Human-readable reason for the recommendation.
    pub reason: String,
}

// ─── Detection Functions ─────────────────────────────────────────────────────

/// Recommend the best runtime for the given hardware profile.
pub fn recommend(hw: &Hardware) -> RuntimeRecommendation {
    match (hw.os, hw.gpu) {
        // macOS Apple Silicon — llama.cpp Metal is the primary choice
        (OsType::MacOs, GpuType::AppleSilicon) => {
            let mut alts = vec![];
            if hw.runtimes.contains(&Runtime::Mlx) {
                alts.push(Runtime::Mlx);
            }
            if hw.runtimes.contains(&Runtime::Ollama) {
                alts.push(Runtime::Ollama);
            }

            RuntimeRecommendation {
                primary: Runtime::LlamaCpp,
                alternatives: alts,
                reason: format!(
                    "macOS Apple Silicon with {}GB unified memory — \
                     llama.cpp Metal for best GGUF performance",
                    hw.memory_gib
                ),
            }
        }

        // NVIDIA CUDA (DGX Spark, etc.) — vLLM for production serving
        (OsType::Linux, GpuType::NvidiaCuda) => {
            let mut alts = vec![Runtime::LlamaCpp];
            if hw.runtimes.contains(&Runtime::Ollama) {
                alts.push(Runtime::Ollama);
            }

            // Check if this is likely a DGX Spark (has HBM or Blackwell GPU)
            let is_dgx = hw
                .gpu_model
                .as_deref()
                .map(|m| m.contains("GB10") || m.contains("Blackwell") || m.contains("GB110"))
                .unwrap_or(false);

            let reason = if is_dgx {
                format!(
                    "DGX Spark with {} — vLLM for high-throughput serving, \
                     tensor parallel available for linked pairs",
                    hw.gpu_model.as_deref().unwrap_or("NVIDIA GPU")
                )
            } else {
                format!(
                    "NVIDIA GPU ({}) — vLLM for production serving",
                    hw.gpu_model.as_deref().unwrap_or("unknown")
                )
            };

            RuntimeRecommendation {
                primary: Runtime::Vllm,
                alternatives: alts,
                reason,
            }
        }

        // AMD RDNA iGPU — llama.cpp with Vulkan acceleration
        (OsType::Linux, GpuType::AmdRdna) => {
            let mut alts = vec![];
            if hw.runtimes.contains(&Runtime::Ollama) {
                alts.push(Runtime::Ollama);
            }

            RuntimeRecommendation {
                primary: Runtime::LlamaCpp,
                alternatives: alts,
                reason: format!(
                    "AMD RDNA iGPU ({}) — llama.cpp with Vulkan acceleration",
                    hw.gpu_model.as_deref().unwrap_or("AMD GPU")
                ),
            }
        }

        // Intel GPU — llama.cpp with Vulkan
        (_, GpuType::IntelGpu) => RuntimeRecommendation {
            primary: Runtime::LlamaCpp,
            alternatives: vec![Runtime::Ollama],
            reason: "Intel GPU — llama.cpp with Vulkan".into(),
        },

        // CPU only (Ubuntu AMD workers without discrete GPU)
        (OsType::Linux, GpuType::None) => {
            let mut alts = vec![];
            if hw.runtimes.contains(&Runtime::Ollama) {
                alts.push(Runtime::Ollama);
            }

            RuntimeRecommendation {
                primary: Runtime::LlamaCpp,
                alternatives: alts,
                reason: format!(
                    "{} with {}GB RAM, no GPU — llama.cpp CPU mode",
                    hw.cpu_model, hw.memory_gib
                ),
            }
        }

        // Fallback
        _ => RuntimeRecommendation {
            primary: Runtime::LlamaCpp,
            alternatives: vec![Runtime::Ollama],
            reason: format!(
                "Unknown configuration ({}, {}) — defaulting to llama.cpp",
                hw.os, hw.gpu
            ),
        },
    }
}

/// Create an `InferenceEngine` for the recommended runtime.
///
/// If `tensor_parallel` > 1 and the recommendation is vLLM, configures
/// tensor parallelism for linked DGX Spark pairs.
pub fn create_engine(hw: &Hardware, tensor_parallel: Option<u32>) -> Box<dyn InferenceEngine> {
    let rec = recommend(hw);

    info!(
        runtime = %rec.primary,
        reason = rec.reason,
        "auto-detected runtime"
    );

    match rec.primary {
        Runtime::LlamaCpp => {
            let backend = LlamaCppBackend::detect(hw.os, hw.gpu);
            Box::new(LlamaCppEngine::new(backend))
        }
        Runtime::Vllm => {
            let tp = tensor_parallel.unwrap_or(1);
            let vllm_config = VllmConfig {
                tensor_parallel_size: tp,
                ..Default::default()
            };
            Box::new(VllmEngine::new(vllm_config))
        }
        Runtime::Mlx => Box::new(MlxEngine::new()),
        Runtime::Ollama => Box::new(OllamaEngine::new()),
        Runtime::TensorRt => {
            // TensorRT-LLM not yet implemented, fall back to vLLM
            info!("TensorRT-LLM not yet supported, falling back to vLLM");
            Box::new(VllmEngine::single_gpu())
        }
    }
}

/// Create an engine for a specific runtime (user override).
pub fn create_engine_for_runtime(
    runtime: Runtime,
    hw: &Hardware,
    tensor_parallel: Option<u32>,
) -> Box<dyn InferenceEngine> {
    match runtime {
        Runtime::LlamaCpp => {
            let backend = LlamaCppBackend::detect(hw.os, hw.gpu);
            Box::new(LlamaCppEngine::new(backend))
        }
        Runtime::Vllm => {
            let tp = tensor_parallel.unwrap_or(1);
            Box::new(VllmEngine::new(VllmConfig {
                tensor_parallel_size: tp,
                ..Default::default()
            }))
        }
        Runtime::Mlx => Box::new(MlxEngine::new()),
        Runtime::Ollama => Box::new(OllamaEngine::new()),
        Runtime::TensorRt => {
            info!("TensorRT-LLM not yet supported, falling back to vLLM");
            Box::new(VllmEngine::single_gpu())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::types::*;

    fn make_hw(os: OsType, gpu: GpuType, mem: u64) -> Hardware {
        Hardware {
            os,
            cpu_model: "Test CPU".into(),
            cpu_cores: 16,
            gpu,
            gpu_model: None,
            memory_gib: mem,
            memory_type: MemoryType::Unknown,
            interconnect: Interconnect::Ethernet10g,
            runtimes: vec![Runtime::LlamaCpp, Runtime::Ollama],
        }
    }

    #[test]
    fn test_recommend_mac() {
        let hw = make_hw(OsType::MacOs, GpuType::AppleSilicon, 128);
        let rec = recommend(&hw);
        assert_eq!(rec.primary, Runtime::LlamaCpp);
        assert!(rec.reason.contains("Apple Silicon"));
    }

    #[test]
    fn test_recommend_nvidia() {
        let hw = make_hw(OsType::Linux, GpuType::NvidiaCuda, 128);
        let rec = recommend(&hw);
        assert_eq!(rec.primary, Runtime::Vllm);
    }

    #[test]
    fn test_recommend_amd() {
        let hw = make_hw(OsType::Linux, GpuType::AmdRdna, 64);
        let rec = recommend(&hw);
        assert_eq!(rec.primary, Runtime::LlamaCpp);
        assert!(rec.reason.contains("Vulkan"));
    }

    #[test]
    fn test_recommend_cpu_only() {
        let hw = make_hw(OsType::Linux, GpuType::None, 64);
        let rec = recommend(&hw);
        assert_eq!(rec.primary, Runtime::LlamaCpp);
        assert!(rec.reason.contains("CPU"));
    }

    #[test]
    fn test_create_engine_mac() {
        let hw = make_hw(OsType::MacOs, GpuType::AppleSilicon, 128);
        let engine = create_engine(&hw, None);
        assert_eq!(engine.name(), "llama.cpp");
    }

    #[test]
    fn test_create_engine_nvidia() {
        let hw = make_hw(OsType::Linux, GpuType::NvidiaCuda, 128);
        let engine = create_engine(&hw, Some(2));
        assert_eq!(engine.name(), "vLLM");
    }
}
