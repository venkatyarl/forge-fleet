//! vLLM runtime manager.
//!
//! Manages `vllm serve` for high-throughput inference on NVIDIA GPUs.
//! Supports tensor parallelism for linked DGX Spark pairs.
//!
//! **Platform:** Linux / DGX OS only.

use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::engine::{EngineConfig, EngineStatus, InferenceEngine};
use crate::error::{Result, RuntimeError};

// ─── vLLM Configuration ─────────────────────────────────────────────────────

/// Additional vLLM-specific configuration.
#[derive(Debug, Clone)]
pub struct VllmConfig {
    /// Number of GPUs for tensor parallelism (1 = single GPU, 2 = linked pair).
    pub tensor_parallel_size: u32,
    /// Maximum number of sequences to serve concurrently.
    pub max_num_seqs: u32,
    /// Data type: "auto", "float16", "bfloat16".
    pub dtype: String,
    /// GPU memory utilization fraction (0.0–1.0).
    pub gpu_memory_utilization: f32,
    /// Trust remote code in HuggingFace models.
    pub trust_remote_code: bool,
    /// Enforce eager mode (no CUDA graphs, useful for debugging).
    pub enforce_eager: bool,
}

impl Default for VllmConfig {
    fn default() -> Self {
        Self {
            tensor_parallel_size: 1,
            max_num_seqs: 64,
            dtype: "auto".into(),
            gpu_memory_utilization: 0.90,
            trust_remote_code: false,
            enforce_eager: false,
        }
    }
}

// ─── VllmEngine ──────────────────────────────────────────────────────────────

/// Manages a `vllm serve` process.
pub struct VllmEngine {
    vllm_config: VllmConfig,
    process: Option<Child>,
    pid: Option<u32>,
    config: Option<EngineConfig>,
    started_at: Option<Instant>,
}

impl VllmEngine {
    /// Create a new vLLM engine manager.
    pub fn new(vllm_config: VllmConfig) -> Self {
        Self {
            vllm_config,
            process: None,
            pid: None,
            config: None,
            started_at: None,
        }
    }

    /// Create with default config for a single DGX Spark.
    pub fn single_gpu() -> Self {
        Self::new(VllmConfig::default())
    }

    /// Create configured for a linked DGX Spark pair (tensor parallel = 2).
    pub fn linked_pair() -> Self {
        Self::new(VllmConfig {
            tensor_parallel_size: 2,
            ..Default::default()
        })
    }

    /// Verify this is running on Linux.
    fn check_platform() -> Result<()> {
        if cfg!(target_os = "linux") {
            Ok(())
        } else {
            Err(RuntimeError::UnsupportedPlatform {
                runtime: "vLLM".into(),
                os: std::env::consts::OS.into(),
            })
        }
    }

    /// Find the vllm binary or Python module.
    fn find_binary() -> Result<String> {
        let output = Command::new("which").arg("vllm").output();
        if let Ok(out) = output
            && out.status.success()
        {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }

        let output = Command::new("python3")
            .args(["-c", "import vllm; print('ok')"])
            .output();
        if let Ok(out) = output
            && out.status.success()
        {
            return Ok("python3 -m vllm".into());
        }

        Err(RuntimeError::BinaryNotFound {
            name: "vllm".into(),
        })
    }

    /// Build arguments for `vllm serve`.
    fn build_args(&self, config: &EngineConfig) -> Vec<String> {
        let model_path = config.model_path.to_string_lossy().into_owned();
        let mut args = vec![
            "serve".into(),
            model_path,
            "--host".into(),
            config.host.clone(),
            "--port".into(),
            config.port.to_string(),
            "--max-model-len".into(),
            config.ctx_size.to_string(),
            "--tensor-parallel-size".into(),
            self.vllm_config.tensor_parallel_size.to_string(),
            "--max-num-seqs".into(),
            self.vllm_config.max_num_seqs.to_string(),
            "--dtype".into(),
            self.vllm_config.dtype.clone(),
            "--gpu-memory-utilization".into(),
            self.vllm_config.gpu_memory_utilization.to_string(),
        ];

        if self.vllm_config.trust_remote_code {
            args.push("--trust-remote-code".into());
        }
        if self.vllm_config.enforce_eager {
            args.push("--enforce-eager".into());
        }

        args.extend(config.extra_args.iter().cloned());
        args
    }

    /// Wait for vLLM health endpoint.
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
                    info!(url, "vLLM server is healthy");
                    return Ok(());
                }
                Ok(resp) => {
                    debug!(status = %resp.status(), "vLLM health check not ready yet");
                }
                Err(e) => {
                    debug!(err = %e, "vLLM health check connection failed (server starting)");
                }
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

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
}

impl InferenceEngine for VllmEngine {
    fn name(&self) -> &str {
        "vLLM"
    }

    fn start(
        &mut self,
        config: &EngineConfig,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u32>> + Send + '_>> {
        let config = config.clone();
        Box::pin(async move {
            Self::check_platform()?;

            if self.is_process_alive()
                && let Some(ref cfg) = self.config
            {
                return Err(RuntimeError::AlreadyRunning { port: cfg.port });
            }

            let binary = Self::find_binary()?;
            let args = self.build_args(&config);

            info!(
                binary = binary,
                tp = self.vllm_config.tensor_parallel_size,
                model = %config.model_path.display(),
                port = config.port,
                "starting vLLM"
            );

            let child = if binary.contains("python") {
                Command::new("python3")
                    .args(["-m", "vllm"])
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            } else {
                Command::new(&binary)
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
            .map_err(|e| RuntimeError::StartFailed {
                reason: format!("failed to spawn vllm: {e}"),
            })?;

            let pid = child.id();
            self.process = Some(child);
            self.pid = Some(pid);
            self.config = Some(config.clone());
            self.started_at = Some(Instant::now());

            info!(pid, "vLLM spawned, waiting for health");
            self.wait_for_health(&config, Duration::from_secs(300))
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
            info!(pid, "stopping vLLM");

            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output();
            }

            let mut child = child;
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        info!(pid, "vLLM stopped gracefully");
                        break;
                    }
                    Ok(None) => {
                        if start.elapsed() > Duration::from_secs(15) {
                            warn!(pid, "vLLM did not stop gracefully, sending SIGKILL");
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => {
                        error!(pid, err = %e, "error waiting for vLLM to stop");
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
    fn test_vllm_config_default() {
        let cfg = VllmConfig::default();
        assert_eq!(cfg.tensor_parallel_size, 1);
        assert_eq!(cfg.dtype, "auto");
    }

    #[test]
    fn test_linked_pair_config() {
        let engine = VllmEngine::linked_pair();
        assert_eq!(engine.vllm_config.tensor_parallel_size, 2);
    }

    #[test]
    fn test_build_args() {
        let engine = VllmEngine::single_gpu();
        let config = EngineConfig {
            model_path: "/models/qwen3-72b".into(),
            model_id: "qwen3-72b".into(),
            host: "0.0.0.0".into(),
            port: 8000,
            ctx_size: 32768,
            gpu_layers: -1,
            parallel: 4,
            extra_args: vec![],
        };
        let args = engine.build_args(&config);
        assert!(args.contains(&"serve".to_string()));
        assert!(args.contains(&"8000".to_string()));
        assert!(args.contains(&"--tensor-parallel-size".to_string()));
    }
}
