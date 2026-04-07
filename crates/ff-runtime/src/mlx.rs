//! MLX runtime manager.
//!
//! Manages `mlx_lm.server` for inference on macOS Apple Silicon.
//! Uses Apple's MLX framework for optimized Metal-accelerated inference.
//!
//! **Platform:** macOS Apple Silicon only.

use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::engine::{EngineConfig, EngineStatus, InferenceEngine};
use crate::error::{Result, RuntimeError};

// ─── MLX Engine ──────────────────────────────────────────────────────────────

/// Manages a `mlx_lm.server` process.
pub struct MlxEngine {
    process: Option<Child>,
    pid: Option<u32>,
    config: Option<EngineConfig>,
    started_at: Option<Instant>,
}

impl MlxEngine {
    /// Create a new MLX engine manager.
    pub fn new() -> Self {
        Self {
            process: None,
            pid: None,
            config: None,
            started_at: None,
        }
    }

    /// Verify we're on macOS Apple Silicon.
    fn check_platform() -> Result<()> {
        if !cfg!(target_os = "macos") {
            return Err(RuntimeError::UnsupportedPlatform {
                runtime: "MLX".into(),
                os: std::env::consts::OS.into(),
            });
        }

        let output = Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let cpu = String::from_utf8_lossy(&out.stdout);
                if !cpu.contains("Apple") {
                    return Err(RuntimeError::UnsupportedPlatform {
                        runtime: "MLX".into(),
                        os: "macOS (Intel — MLX requires Apple Silicon)".into(),
                    });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Check if mlx_lm is installed.
    fn check_mlx_installed() -> Result<()> {
        let output = Command::new("python3")
            .args(["-c", "import mlx_lm; print('ok')"])
            .output();

        match output {
            Ok(out) if out.status.success() => Ok(()),
            _ => Err(RuntimeError::BinaryNotFound {
                name: "mlx_lm (Python package — install via: pip install mlx-lm)".into(),
            }),
        }
    }

    /// Build arguments for `python3 -m mlx_lm.server`.
    fn build_args(&self, config: &EngineConfig) -> Vec<String> {
        let mut args = vec![
            "-m".into(),
            "mlx_lm.server".into(),
            "--model".into(),
            config.model_path.to_string_lossy().into_owned(),
            "--host".into(),
            config.host.clone(),
            "--port".into(),
            config.port.to_string(),
        ];

        args.extend(config.extra_args.iter().cloned());
        args
    }

    /// Wait for the health endpoint.
    async fn wait_for_health(&self, config: &EngineConfig, timeout: Duration) -> Result<()> {
        let url = format!("http://{}:{}/v1/models", config.host, config.port);
        let client = reqwest::Client::new();
        let start = Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(RuntimeError::HealthTimeout);
            }

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(url, "MLX server is healthy");
                    return Ok(());
                }
                Ok(resp) => {
                    debug!(status = %resp.status(), "MLX health check not ready yet");
                }
                Err(e) => {
                    debug!(err = %e, "MLX health check connection failed (server starting)");
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
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

impl Default for MlxEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceEngine for MlxEngine {
    fn name(&self) -> &str {
        "MLX"
    }

    fn start(
        &mut self,
        config: &EngineConfig,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u32>> + Send + '_>> {
        let config = config.clone();
        Box::pin(async move {
            Self::check_platform()?;
            Self::check_mlx_installed()?;

            if self.is_process_alive()
                && let Some(ref cfg) = self.config
            {
                return Err(RuntimeError::AlreadyRunning { port: cfg.port });
            }

            let args = self.build_args(&config);

            info!(
                model = %config.model_path.display(),
                port = config.port,
                "starting MLX server"
            );

            let child = Command::new("python3")
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| RuntimeError::StartFailed {
                    reason: format!("failed to spawn mlx_lm.server: {e}"),
                })?;

            let pid = child.id();
            self.process = Some(child);
            self.pid = Some(pid);
            self.config = Some(config.clone());
            self.started_at = Some(Instant::now());

            info!(pid, "MLX server spawned, waiting for health check");
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
            info!(pid, "stopping MLX server");

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
                        info!(pid, "MLX server stopped gracefully");
                        break;
                    }
                    Ok(None) => {
                        if start.elapsed() > Duration::from_secs(10) {
                            warn!(pid, "MLX server did not stop gracefully, sending SIGKILL");
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    Err(e) => {
                        error!(pid, err = %e, "error waiting for MLX server to stop");
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

            let url = format!("http://{}:{}/v1/models", config.host, config.port);
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
    fn test_mlx_default() {
        let engine = MlxEngine::default();
        assert!(engine.pid.is_none());
        assert!(engine.config.is_none());
    }

    #[test]
    fn test_build_args() {
        let engine = MlxEngine::new();
        let config = EngineConfig {
            model_path: "mlx-community/Qwen3-32B-4bit".into(),
            model_id: "qwen3-32b-mlx".into(),
            host: "0.0.0.0".into(),
            port: 51801,
            ctx_size: 8192,
            gpu_layers: -1,
            parallel: 4,
            extra_args: vec![],
        };
        let args = engine.build_args(&config);
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"mlx_lm.server".to_string()));
        assert!(args.contains(&"51801".to_string()));
    }
}
