//! llama.cpp runtime manager.
//!
//! Manages the `llama-server` process with correct backend flags for:
//! - **Metal** (macOS Apple Silicon)
//! - **CUDA** (NVIDIA / DGX Spark)
//! - **Vulkan** (AMD RDNA iGPU)
//! - **CPU** (fallback)
//!
//! Handles GGUF model files exclusively.

use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use ff_core::types::{GpuType, OsType};

use crate::engine::{EngineConfig, EngineStatus, InferenceEngine};
use crate::error::{Result, RuntimeError};

// ─── Backend Selection ───────────────────────────────────────────────────────

/// Which acceleration backend llama-server should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlamaCppBackend {
    /// Apple Metal (macOS Apple Silicon).
    Metal,
    /// NVIDIA CUDA.
    Cuda,
    /// Vulkan (AMD, Intel, or cross-platform).
    Vulkan,
    /// Pure CPU — no GPU acceleration.
    Cpu,
}

impl LlamaCppBackend {
    /// Select the best backend for the given hardware.
    pub fn detect(os: OsType, gpu: GpuType) -> Self {
        match (os, gpu) {
            (OsType::MacOs, GpuType::AppleSilicon) => Self::Metal,
            (_, GpuType::NvidiaCuda) => Self::Cuda,
            (_, GpuType::AmdRdna) => Self::Vulkan,
            (_, GpuType::IntelGpu) => Self::Vulkan,
            _ => Self::Cpu,
        }
    }

    /// Return env overrides for this backend.
    fn env_overrides(&self) -> Vec<(&str, &str)> {
        match self {
            Self::Metal => vec![],
            Self::Cuda => vec![],
            Self::Vulkan => vec![("GGML_VULKAN", "1")],
            Self::Cpu => vec![("GGML_NO_METAL", "1"), ("CUDA_VISIBLE_DEVICES", "")],
        }
    }
}

impl std::fmt::Display for LlamaCppBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Metal => write!(f, "Metal"),
            Self::Cuda => write!(f, "CUDA"),
            Self::Vulkan => write!(f, "Vulkan"),
            Self::Cpu => write!(f, "CPU"),
        }
    }
}

// ─── LlamaCpp Engine ─────────────────────────────────────────────────────────

/// Manages a `llama-server` process.
pub struct LlamaCppEngine {
    backend: LlamaCppBackend,
    process: Option<Child>,
    pid: Option<u32>,
    config: Option<EngineConfig>,
    started_at: Option<Instant>,
}

impl LlamaCppEngine {
    /// Create a new llama.cpp engine manager.
    pub fn new(backend: LlamaCppBackend) -> Self {
        Self {
            backend,
            process: None,
            pid: None,
            config: None,
            started_at: None,
        }
    }

    /// Create with auto-detected backend for the current hardware.
    pub fn auto_detect(os: OsType, gpu: GpuType) -> Self {
        Self::new(LlamaCppBackend::detect(os, gpu))
    }

    /// Find the llama-server binary on PATH.
    fn find_binary() -> Result<PathBuf> {
        let candidates = [
            "llama-server",
            "/usr/local/bin/llama-server",
            "/opt/homebrew/bin/llama-server",
        ];

        for candidate in &candidates {
            let output = Command::new("which").arg(candidate).output();
            if let Ok(out) = output
                && out.status.success()
            {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                return Ok(PathBuf::from(path));
            }
        }

        Err(RuntimeError::BinaryNotFound {
            name: "llama-server".into(),
        })
    }

    /// Build the command-line arguments for llama-server.
    fn build_args(&self, config: &EngineConfig) -> Vec<String> {
        let mut args = vec![
            "--model".into(),
            config.model_path.to_string_lossy().into_owned(),
            "--host".into(),
            config.host.clone(),
            "--port".into(),
            config.port.to_string(),
            "--ctx-size".into(),
            config.ctx_size.to_string(),
            "--parallel".into(),
            config.parallel.to_string(),
        ];

        if config.gpu_layers >= 0 {
            args.push("--n-gpu-layers".into());
            args.push(config.gpu_layers.to_string());
        } else {
            args.push("--n-gpu-layers".into());
            args.push("999".into());
        }

        args.extend(config.extra_args.iter().cloned());
        args
    }

    /// Wait for the health endpoint to respond, up to a timeout.
    async fn wait_for_health(&self, config: &EngineConfig, timeout: Duration) -> Result<()> {
        let url = format!("http://{}:{}/health", config.host, config.port);
        let client = reqwest::Client::new();
        let start = Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(RuntimeError::HealthTimeout);
            }

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(url, "llama-server is healthy");
                    return Ok(());
                }
                Ok(resp) => {
                    debug!(status = %resp.status(), "health check not ready yet");
                }
                Err(e) => {
                    debug!(err = %e, "health check connection failed (server starting)");
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Check if the process is still alive.
    fn is_process_alive(&mut self) -> bool {
        match &mut self.process {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(_) => false,
            },
            None => false,
        }
    }

    /// Send SIGTERM to a process (Unix only).
    #[cfg(unix)]
    fn send_sigterm(pid: u32) {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();
    }
}

impl InferenceEngine for LlamaCppEngine {
    fn name(&self) -> &str {
        "llama.cpp"
    }

    fn start(
        &mut self,
        config: &EngineConfig,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u32>> + Send + '_>> {
        let config = config.clone();
        Box::pin(async move {
            if self.is_process_alive()
                && let Some(ref cfg) = self.config
            {
                return Err(RuntimeError::AlreadyRunning { port: cfg.port });
            }

            if !config.model_path.exists() {
                return Err(RuntimeError::ModelNotFound {
                    path: config.model_path.clone(),
                });
            }

            let ext = config
                .model_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if ext != "gguf" {
                return Err(RuntimeError::StartFailed {
                    reason: format!(
                        "llama.cpp requires GGUF models, got: {}",
                        config.model_path.display()
                    ),
                });
            }

            let binary = Self::find_binary()?;
            let args = self.build_args(&config);

            info!(
                backend = %self.backend,
                binary = %binary.display(),
                model = %config.model_path.display(),
                port = config.port,
                "starting llama-server"
            );

            let mut cmd = Command::new(&binary);
            cmd.args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            for (key, val) in self.backend.env_overrides() {
                cmd.env(key, val);
            }

            let child = cmd.spawn().map_err(|e| RuntimeError::StartFailed {
                reason: format!("failed to spawn llama-server: {e}"),
            })?;

            let pid = child.id();
            self.process = Some(child);
            self.pid = Some(pid);
            self.config = Some(config.clone());
            self.started_at = Some(Instant::now());

            info!(pid, "llama-server spawned, waiting for health check");

            self.wait_for_health(&config, Duration::from_secs(120))
                .await?;

            Ok(pid)
        })
    }

    fn stop(&mut self) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let child = match self.process.take() {
                Some(c) => c,
                None => return Err(RuntimeError::NotRunning),
            };

            let pid = child.id();
            info!(pid, "stopping llama-server");

            #[cfg(unix)]
            Self::send_sigterm(pid);

            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }

            let mut child = child;
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        info!(pid, "llama-server stopped gracefully");
                        break;
                    }
                    Ok(None) => {
                        if start.elapsed() > Duration::from_secs(10) {
                            warn!(pid, "llama-server did not stop gracefully, sending SIGKILL");
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    Err(e) => {
                        error!(pid, err = %e, "error waiting for llama-server to stop");
                        break;
                    }
                }
            }

            self.pid = None;
            self.config = None;
            self.started_at = None;
            Ok(())
        })
    }

    fn health_check(&self) -> Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let config = match &self.config {
                Some(c) => c,
                None => return Ok(false),
            };

            let url = format!("http://{}:{}/health", config.host, config.port);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| RuntimeError::HealthCheckFailed {
                    reason: e.to_string(),
                })?;

            match client.get(&url).send().await {
                Ok(resp) => Ok(resp.status().is_success()),
                Err(_) => Ok(false),
            }
        })
    }

    fn get_models(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<String>>> + Send + '_>> {
        Box::pin(async move {
            let config = match &self.config {
                Some(c) => c,
                None => return Ok(vec![]),
            };

            let url = format!("http://{}:{}/v1/models", config.host, config.port);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| RuntimeError::Other(e.to_string()))?;

            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            let models = body
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();

            Ok(models)
        })
    }

    fn get_endpoint(&self) -> Option<String> {
        self.config
            .as_ref()
            .map(|c| format!("http://{}:{}", c.host, c.port))
    }

    fn status(&self) -> Pin<Box<dyn std::future::Future<Output = EngineStatus> + Send + '_>> {
        Box::pin(async move {
            let healthy = self.health_check().await.unwrap_or(false);
            let uptime = self.started_at.map(|s| s.elapsed().as_secs());

            EngineStatus {
                running: self.pid.is_some(),
                healthy,
                pid: self.pid,
                model_id: self.config.as_ref().map(|c| c.model_id.clone()),
                endpoint: self.get_endpoint(),
                uptime_secs: uptime,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_detect_macos_apple_silicon() {
        let backend = LlamaCppBackend::detect(OsType::MacOs, GpuType::AppleSilicon);
        assert_eq!(backend, LlamaCppBackend::Metal);
    }

    #[test]
    fn test_backend_detect_nvidia() {
        let backend = LlamaCppBackend::detect(OsType::Linux, GpuType::NvidiaCuda);
        assert_eq!(backend, LlamaCppBackend::Cuda);
    }

    #[test]
    fn test_backend_detect_amd() {
        let backend = LlamaCppBackend::detect(OsType::Linux, GpuType::AmdRdna);
        assert_eq!(backend, LlamaCppBackend::Vulkan);
    }

    #[test]
    fn test_backend_detect_cpu_only() {
        let backend = LlamaCppBackend::detect(OsType::Linux, GpuType::None);
        assert_eq!(backend, LlamaCppBackend::Cpu);
    }

    #[test]
    fn test_build_args() {
        let engine = LlamaCppEngine::new(LlamaCppBackend::Metal);
        let config = EngineConfig {
            model_path: "/models/test.gguf".into(),
            model_id: "test-model".into(),
            host: "0.0.0.0".into(),
            port: 51800,
            ctx_size: 8192,
            gpu_layers: -1,
            parallel: 4,
            extra_args: vec![],
        };
        let args = engine.build_args(&config);
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"51800".to_string()));
        assert!(args.contains(&"999".to_string()));
    }
}
