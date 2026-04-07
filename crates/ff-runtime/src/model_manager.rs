//! Model file management — download, track, and manage model files.
//!
//! Handles:
//! - Downloading GGUF files (for llama.cpp) from URLs
//! - Downloading safetensors (for vLLM) from HuggingFace
//! - Tracking which models live on which nodes
//! - Basic quantization dispatch (via llama-quantize)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use ff_core::types::Runtime;

use crate::error::{Result, RuntimeError};

// ─── Model Registry ──────────────────────────────────────────────────────────

/// Metadata for a managed model file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedModel {
    /// Model identifier (e.g. "qwen3-32b-q4_k_m").
    pub id: String,
    /// Display name.
    pub name: String,
    /// Path to the model file on disk.
    pub path: PathBuf,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Runtime this model is for.
    pub runtime: Runtime,
    /// Quantization level (e.g. "Q4_K_M", "FP16").
    pub quant: String,
    /// Source URL the model was downloaded from.
    pub source_url: Option<String>,
    /// When the model was downloaded/registered.
    pub downloaded_at: DateTime<Utc>,
    /// SHA-256 hash of the file (if computed).
    pub sha256: Option<String>,
    /// Node names this model is available on.
    pub nodes: Vec<String>,
}

/// Manages model files across the fleet.
pub struct ModelManager {
    /// Base directory for storing models.
    models_dir: PathBuf,
    /// Registry of known models (model_id → metadata).
    registry: HashMap<String, ManagedModel>,
}

impl ModelManager {
    /// Create a new model manager with the given base directory.
    pub fn new(models_dir: PathBuf) -> Self {
        Self {
            models_dir,
            registry: HashMap::new(),
        }
    }

    /// Ensure the models directory exists.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.models_dir)?;
        Ok(())
    }

    /// Get the models directory.
    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    /// Register a model that already exists on disk.
    pub fn register(&mut self, model: ManagedModel) {
        info!(
            id = model.id,
            path = %model.path.display(),
            runtime = %model.runtime,
            "registered model"
        );
        self.registry.insert(model.id.clone(), model);
    }

    /// Get a model by ID.
    pub fn get(&self, model_id: &str) -> Option<&ManagedModel> {
        self.registry.get(model_id)
    }

    /// List all registered models.
    pub fn list(&self) -> Vec<&ManagedModel> {
        self.registry.values().collect()
    }

    /// List models for a specific runtime.
    pub fn list_for_runtime(&self, runtime: Runtime) -> Vec<&ManagedModel> {
        self.registry
            .values()
            .filter(|m| m.runtime == runtime)
            .collect()
    }

    /// List models available on a specific node.
    pub fn list_for_node(&self, node_name: &str) -> Vec<&ManagedModel> {
        self.registry
            .values()
            .filter(|m| m.nodes.iter().any(|n| n == node_name))
            .collect()
    }

    /// Check if a model exists on disk.
    pub fn exists_on_disk(&self, model_id: &str) -> bool {
        self.registry
            .get(model_id)
            .map(|m| m.path.exists())
            .unwrap_or(false)
    }

    /// Remove a model from the registry (does NOT delete the file).
    pub fn unregister(&mut self, model_id: &str) -> Option<ManagedModel> {
        let model = self.registry.remove(model_id);
        if let Some(ref m) = model {
            info!(id = m.id, "unregistered model");
        }
        model
    }

    /// Delete a model file from disk and unregister it.
    pub fn delete(&mut self, model_id: &str) -> Result<()> {
        if let Some(model) = self.registry.remove(model_id)
            && model.path.exists()
        {
            std::fs::remove_file(&model.path)?;
            info!(
                id = model.id,
                path = %model.path.display(),
                "deleted model file"
            );
        }
        Ok(())
    }

    // ─── Download ────────────────────────────────────────────────────────

    /// Download a GGUF model file from a URL.
    ///
    /// Uses `curl` for robust downloading with resume support.
    pub async fn download_gguf(
        &mut self,
        url: &str,
        model_id: &str,
        model_name: &str,
        quant: &str,
        node_name: &str,
    ) -> Result<ManagedModel> {
        self.init()?;

        // Derive filename from URL
        let fallback = format!("{model_id}.gguf");
        let filename = url.rsplit('/').next().unwrap_or(&fallback);
        let dest = self.models_dir.join(filename);

        if dest.exists() {
            info!(
                path = %dest.display(),
                "model file already exists, skipping download"
            );
        } else {
            info!(
                url = url,
                dest = %dest.display(),
                "downloading GGUF model"
            );

            // Use curl for download (supports resume, progress, retries)
            let output = Command::new("curl")
                .args([
                    "-L", // follow redirects
                    "-C",
                    "-", // resume if possible
                    "--retry",
                    "3", // retry on failure
                    "--retry-delay",
                    "5",
                    "-o",
                    &dest.to_string_lossy(),
                    url,
                ])
                .output()
                .map_err(|e| RuntimeError::DownloadFailed {
                    reason: format!("failed to run curl: {e}"),
                })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(RuntimeError::DownloadFailed {
                    reason: format!("curl failed: {stderr}"),
                });
            }

            info!(path = %dest.display(), "download complete");
        }

        // Get file size
        let size_bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);

        let model = ManagedModel {
            id: model_id.into(),
            name: model_name.into(),
            path: dest,
            size_bytes,
            runtime: Runtime::LlamaCpp,
            quant: quant.into(),
            source_url: Some(url.into()),
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec![node_name.into()],
        };

        self.register(model.clone());
        Ok(model)
    }

    /// Clone/download a HuggingFace model for vLLM (safetensors).
    ///
    /// Uses `huggingface-cli download` or falls back to `git clone`.
    pub async fn download_hf_model(
        &mut self,
        repo_id: &str,
        model_id: &str,
        model_name: &str,
        node_name: &str,
    ) -> Result<ManagedModel> {
        self.init()?;

        let dest = self.models_dir.join(model_id);

        if dest.exists() {
            info!(
                path = %dest.display(),
                "HuggingFace model directory already exists"
            );
        } else {
            info!(repo = repo_id, dest = %dest.display(), "downloading HuggingFace model");

            // Try huggingface-cli first
            let hf_result = Command::new("huggingface-cli")
                .args(["download", repo_id, "--local-dir", &dest.to_string_lossy()])
                .output();

            match hf_result {
                Ok(out) if out.status.success() => {
                    info!("downloaded via huggingface-cli");
                }
                _ => {
                    // Fall back to git clone
                    warn!("huggingface-cli not available, trying git clone");
                    let git_url = format!("https://huggingface.co/{repo_id}");
                    let output = Command::new("git")
                        .args(["clone", "--depth", "1", &git_url, &dest.to_string_lossy()])
                        .output()
                        .map_err(|e| RuntimeError::DownloadFailed {
                            reason: format!("git clone failed: {e}"),
                        })?;

                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(RuntimeError::DownloadFailed {
                            reason: format!("git clone failed: {stderr}"),
                        });
                    }
                }
            }
        }

        // Calculate total size of the directory
        let size_bytes = dir_size(&dest).unwrap_or(0);

        let model = ManagedModel {
            id: model_id.into(),
            name: model_name.into(),
            path: dest,
            size_bytes,
            runtime: Runtime::Vllm,
            quant: "FP16".into(),
            source_url: Some(format!("https://huggingface.co/{repo_id}")),
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec![node_name.into()],
        };

        self.register(model.clone());
        Ok(model)
    }

    // ─── Quantization ────────────────────────────────────────────────────

    /// Quantize a GGUF model to a different quantization level.
    ///
    /// Uses `llama-quantize` from the llama.cpp toolkit.
    pub fn quantize(&self, source_path: &Path, output_path: &Path, quant_type: &str) -> Result<()> {
        info!(
            src = %source_path.display(),
            dst = %output_path.display(),
            quant = quant_type,
            "quantizing model"
        );

        let output = Command::new("llama-quantize")
            .args([
                source_path.to_string_lossy().as_ref(),
                output_path.to_string_lossy().as_ref(),
                quant_type,
            ])
            .output()
            .map_err(|e| RuntimeError::QuantizationFailed {
                reason: format!("failed to run llama-quantize: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(RuntimeError::QuantizationFailed {
                reason: format!("llama-quantize failed: {stderr}"),
            });
        }

        info!(
            output = %output_path.display(),
            "quantization complete"
        );

        Ok(())
    }

    // ─── Persistence ─────────────────────────────────────────────────────

    /// Save the registry to a JSON file.
    pub fn save_registry(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.registry)?;
        std::fs::write(path, json)?;
        info!(path = %path.display(), "saved model registry");
        Ok(())
    }

    /// Load the registry from a JSON file.
    pub fn load_registry(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            info!(path = %path.display(), "no registry file found, starting fresh");
            return Ok(());
        }

        let content = std::fs::read_to_string(path)?;
        self.registry = serde_json::from_str(&content)?;
        info!(
            path = %path.display(),
            count = self.registry.len(),
            "loaded model registry"
        );
        Ok(())
    }

    /// Get total disk usage of all registered models (in bytes).
    pub fn total_disk_usage(&self) -> u64 {
        self.registry.values().map(|m| m.size_bytes).sum()
    }

    /// Get total disk usage formatted as a human-readable string.
    pub fn total_disk_usage_human(&self) -> String {
        let bytes = self.total_disk_usage();
        if bytes >= 1_000_000_000_000 {
            format!("{:.1} TB", bytes as f64 / 1_000_000_000_000.0)
        } else if bytes >= 1_000_000_000 {
            format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
        } else if bytes >= 1_000_000 {
            format!("{:.1} MB", bytes as f64 / 1_000_000.0)
        } else {
            format!("{} bytes", bytes)
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Calculate the total size of a directory (recursive).
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                total += dir_size(&entry.path())?;
            } else {
                total += metadata.len();
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_manager_new() {
        let mm = ModelManager::new("/tmp/ff-models".into());
        assert!(mm.list().is_empty());
        assert_eq!(mm.total_disk_usage(), 0);
    }

    #[test]
    fn test_register_and_get() {
        let mut mm = ModelManager::new("/tmp/ff-models".into());
        let model = ManagedModel {
            id: "test-model".into(),
            name: "Test Model".into(),
            path: "/tmp/test.gguf".into(),
            size_bytes: 1_000_000,
            runtime: Runtime::LlamaCpp,
            quant: "Q4_K_M".into(),
            source_url: None,
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec!["taylor".into()],
        };

        mm.register(model);
        assert!(mm.get("test-model").is_some());
        assert_eq!(mm.list().len(), 1);
        assert_eq!(mm.total_disk_usage(), 1_000_000);
    }

    #[test]
    fn test_list_for_runtime() {
        let mut mm = ModelManager::new("/tmp/ff-models".into());

        mm.register(ManagedModel {
            id: "gguf-model".into(),
            name: "GGUF Model".into(),
            path: "/tmp/model.gguf".into(),
            size_bytes: 5_000_000_000,
            runtime: Runtime::LlamaCpp,
            quant: "Q4_K_M".into(),
            source_url: None,
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec!["taylor".into()],
        });

        mm.register(ManagedModel {
            id: "vllm-model".into(),
            name: "vLLM Model".into(),
            path: "/tmp/vllm-model".into(),
            size_bytes: 10_000_000_000,
            runtime: Runtime::Vllm,
            quant: "FP16".into(),
            source_url: None,
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec!["sia".into()],
        });

        assert_eq!(mm.list_for_runtime(Runtime::LlamaCpp).len(), 1);
        assert_eq!(mm.list_for_runtime(Runtime::Vllm).len(), 1);
        assert_eq!(mm.list_for_runtime(Runtime::Mlx).len(), 0);
    }

    #[test]
    fn test_list_for_node() {
        let mut mm = ModelManager::new("/tmp/ff-models".into());

        mm.register(ManagedModel {
            id: "model-a".into(),
            name: "Model A".into(),
            path: "/tmp/a.gguf".into(),
            size_bytes: 1_000,
            runtime: Runtime::LlamaCpp,
            quant: "Q4_K_M".into(),
            source_url: None,
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec!["taylor".into(), "james".into()],
        });

        assert_eq!(mm.list_for_node("taylor").len(), 1);
        assert_eq!(mm.list_for_node("james").len(), 1);
        assert_eq!(mm.list_for_node("marcus").len(), 0);
    }

    #[test]
    fn test_unregister() {
        let mut mm = ModelManager::new("/tmp/ff-models".into());

        mm.register(ManagedModel {
            id: "to-remove".into(),
            name: "Remove Me".into(),
            path: "/tmp/remove.gguf".into(),
            size_bytes: 1_000,
            runtime: Runtime::LlamaCpp,
            quant: "Q4_K_M".into(),
            source_url: None,
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec![],
        });

        assert!(mm.get("to-remove").is_some());
        let removed = mm.unregister("to-remove");
        assert!(removed.is_some());
        assert!(mm.get("to-remove").is_none());
    }

    #[test]
    fn test_disk_usage_human() {
        let mut mm = ModelManager::new("/tmp/ff-models".into());

        mm.register(ManagedModel {
            id: "big-model".into(),
            name: "Big Model".into(),
            path: "/tmp/big.gguf".into(),
            size_bytes: 15_000_000_000,
            runtime: Runtime::LlamaCpp,
            quant: "Q4_K_M".into(),
            source_url: None,
            downloaded_at: Utc::now(),
            sha256: None,
            nodes: vec![],
        });

        let human = mm.total_disk_usage_human();
        assert!(human.contains("GB"));
    }
}
