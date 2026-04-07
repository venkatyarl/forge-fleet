//! Ollama runtime manager.
//!
//! Manages `ollama serve` and `ollama pull` for easy model setup.
//! This is the fallback runtime — works everywhere, simple setup,
//! but lower performance than llama.cpp or vLLM.

use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::engine::{EngineConfig, EngineStatus, InferenceEngine};
use crate::error::{Result, RuntimeError};

// ─── Ollama Engine ───────────────────────────────────────────────────────────

/// Manages an Ollama server process.
pub struct OllamaEngine {
    process: Option<Child>,
    pid: Option<u32>,
    config: Option<EngineConfig>,
    started_at: Option<Instant>,
    /// Whether we started Ollama ourselves vs. it was already running.
    externally_managed: bool,
}

impl OllamaEngine {
    /// Create a new Ollama engine manager.
    pub fn new() -> Self {
        Self {
            process: None,
            pid: None,
            config: None,
            started_at: None,
            externally_managed: false,
        }
    }

    /// Check if the ollama binary is available.
    fn check_binary() -> Result<()> {
        let output = Command::new("which").arg("ollama").output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            _ => Err(RuntimeError::BinaryNotFound {
                name: "ollama (install from https://ollama.ai)".into(),
            }),
        }
    }

    /// Pull a model if not already present.
    pub fn pull_model(model_name: &str) -> Result<()> {
        info!(model = model_name, "pulling Ollama model");

        let output = Command::new("ollama")
            .args(["pull", model_name])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| RuntimeError::DownloadFailed {
                reason: format!("failed to run ollama pull: {e}"),
            })?;

        if !output.status.success() {
            return Err(RuntimeError::DownloadFailed {
                reason: format!(
                    "ollama pull {} failed with exit code: {:?}",
                    model_name,
                    output.status.code()
                ),
            });
        }

        info!(model = model_name, "model pulled successfully");
        Ok(())
    }

    /// List locally available models.
    pub fn list_local_models() -> Result<Vec<String>> {
        let output = Command::new("ollama")
            .args(["list"])
            .output()
            .map_err(|e| RuntimeError::Other(format!("failed to run ollama list: {e}")))?;

        if !output.status.success() {
            return Ok(vec![]);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let models: Vec<String> = stdout
            .lines()
            .skip(1)
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.first().map(|s| s.to_string())
            })
            .collect();

        Ok(models)
    }

    /// Check if Ollama is already running (e.g. as a system service).
    fn is_ollama_running() -> bool {
        // Use a synchronous TCP check instead of reqwest::blocking
        use std::net::TcpStream;
        TcpStream::connect_timeout(&"127.0.0.1:11434".parse().unwrap(), Duration::from_secs(2))
            .is_ok()
    }

    /// Wait for the health endpoint.
    async fn wait_for_health(&self, host: &str, port: u16, timeout: Duration) -> Result<()> {
        let url = format!("http://{}:{}/api/tags", host, port);
        let client = reqwest::Client::new();
        let start = Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(RuntimeError::HealthTimeout);
            }

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(url, "Ollama server is healthy");
                    return Ok(());
                }
                Ok(resp) => {
                    debug!(status = %resp.status(), "Ollama health check not ready yet");
                }
                Err(e) => {
                    debug!(err = %e, "Ollama health check connection failed");
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    fn is_process_alive(&mut self) -> bool {
        if self.externally_managed {
            return Self::is_ollama_running();
        }

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

impl Default for OllamaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceEngine for OllamaEngine {
    fn name(&self) -> &str {
        "Ollama"
    }

    fn start(
        &mut self,
        config: &EngineConfig,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u32>> + Send + '_>> {
        let config = config.clone();
        Box::pin(async move {
            Self::check_binary()?;

            if self.is_process_alive()
                && let Some(ref cfg) = self.config
            {
                return Err(RuntimeError::AlreadyRunning { port: cfg.port });
            }

            let host = &config.host;
            let port = config.port;

            // Check if Ollama is already running as a system service
            if Self::is_ollama_running() {
                info!("Ollama is already running (system service), using existing instance");
                self.externally_managed = true;
                self.config = Some(config.clone());
                self.started_at = Some(Instant::now());
                self.pid = Some(0);
                return Ok(0);
            }

            info!(host, port, "starting Ollama server");

            let child = Command::new("ollama")
                .arg("serve")
                .env("OLLAMA_HOST", format!("{host}:{port}"))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| RuntimeError::StartFailed {
                    reason: format!("failed to spawn ollama serve: {e}"),
                })?;

            let pid = child.id();
            self.process = Some(child);
            self.pid = Some(pid);
            self.config = Some(config.clone());
            self.started_at = Some(Instant::now());

            info!(pid, "Ollama spawned, waiting for health check");
            self.wait_for_health(host, port, Duration::from_secs(30))
                .await?;

            Ok(pid)
        })
    }

    fn stop(&mut self) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            if self.externally_managed {
                info!("Ollama is externally managed — not stopping");
                self.pid = None;
                self.config = None;
                self.started_at = None;
                self.externally_managed = false;
                return Ok(());
            }

            let child = match self.process.take() {
                Some(c) => c,
                None => return Err(RuntimeError::NotRunning),
            };

            let pid = child.id();
            info!(pid, "stopping Ollama");

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
                        info!(pid, "Ollama stopped gracefully");
                        break;
                    }
                    Ok(None) => {
                        if start.elapsed() > Duration::from_secs(10) {
                            warn!(pid, "Ollama did not stop gracefully, sending SIGKILL");
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    Err(e) => {
                        error!(pid, err = %e, "error waiting for Ollama to stop");
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

            let url = format!("http://{}:{}/api/tags", config.host, config.port);
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

            let url = format!("http://{}:{}/api/tags", config.host, config.port);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| RuntimeError::Other(e.to_string()))?;

            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            let models = body
                .get("models")
                .and_then(|m| m.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
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
                running: self.pid.is_some() || self.externally_managed,
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
    fn test_ollama_default() {
        let engine = OllamaEngine::default();
        assert!(engine.pid.is_none());
        assert!(!engine.externally_managed);
    }
}
