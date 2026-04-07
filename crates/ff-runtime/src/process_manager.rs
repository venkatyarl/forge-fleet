//! Process manager for llama-server instances.
//!
//! Provides higher-level lifecycle management on top of [`LlamaCppEngine`]:
//!
//! - **Detection** — scan running `llama-server` processes via `ps aux`
//! - **Adoption** — claim existing processes on expected ports
//! - **Health monitoring** — periodic HTTP `/health` probes
//! - **Auto-restart** — restart crashed models after N consecutive failures
//! - **Start / Stop** — spawn or terminate `llama-server` with correct args

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::engine::EngineConfig;
use crate::error::{Result, RuntimeError};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the [`ProcessManager`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessManagerConfig {
    /// Restart a model after this many consecutive health-check failures.
    pub max_health_failures: u32,
    /// Seconds between health-check sweeps.
    pub health_check_interval_secs: u64,
    /// Seconds to wait for graceful SIGTERM before sending SIGKILL.
    pub stop_timeout_secs: u64,
    /// Timeout for the HTTP `/health` probe.
    pub health_probe_timeout_secs: u64,
}

impl Default for ProcessManagerConfig {
    fn default() -> Self {
        Self {
            max_health_failures: 3,
            health_check_interval_secs: 30,
            stop_timeout_secs: 10,
            health_probe_timeout_secs: 5,
        }
    }
}

// ─── Detected Process ────────────────────────────────────────────────────────

/// A llama-server process discovered via `ps aux`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedProcess {
    /// OS process ID.
    pub pid: u32,
    /// Port parsed from `--port <N>` argument (if found).
    pub port: Option<u16>,
    /// Model path parsed from `--model <path>` argument (if found).
    pub model_path: Option<String>,
    /// Full command line as reported by `ps`.
    pub cmd_line: String,
}

/// Parse the output of `ps aux` (or similar) looking for `llama-server` lines.
///
/// Each matching line is parsed to extract PID, `--port`, and `--model` args.
pub fn parse_ps_output(ps_output: &str) -> Vec<DetectedProcess> {
    ps_output
        .lines()
        .filter(|line| line.contains("llama-server") || line.contains("llama_server"))
        .filter(|line| !line.contains("grep"))
        .filter_map(parse_ps_line)
        .collect()
}

/// Parse a single `ps aux` line into a [`DetectedProcess`].
///
/// Expected format: `USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND...`
fn parse_ps_line(line: &str) -> Option<DetectedProcess> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 11 {
        return None;
    }

    let pid: u32 = fields[1].parse().ok()?;
    let cmd_line = fields[10..].join(" ");

    // Parse --port / -p flag
    let port =
        extract_flag_value(&fields[10..], &["--port", "-p"]).and_then(|v| v.parse::<u16>().ok());

    // Parse --model / -m flag
    let model_path = extract_flag_value(&fields[10..], &["--model", "-m"]).map(|s| s.to_string());

    Some(DetectedProcess {
        pid,
        port,
        model_path,
        cmd_line,
    })
}

/// Extract the value following any of `flags` in a token list.
fn extract_flag_value<'a>(tokens: &[&'a str], flags: &[&str]) -> Option<&'a str> {
    for (i, token) in tokens.iter().enumerate() {
        if flags.contains(token) {
            return tokens.get(i + 1).copied();
        }
    }
    None
}

/// Run `ps aux` and return all detected llama-server processes.
pub fn detect_running_processes() -> Result<Vec<DetectedProcess>> {
    let output = Command::new("ps")
        .args(["aux"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| RuntimeError::Other(format!("failed to run `ps aux`: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ps_output(&stdout))
}

// ─── Managed Model ───────────────────────────────────────────────────────────

/// A model instance managed by the [`ProcessManager`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedModel {
    /// Engine configuration used to start (or re-start) this model.
    pub config: EngineConfig,
    /// OS PID of the running process (if any).
    pub pid: Option<u32>,
    /// Whether the last health check succeeded.
    pub healthy: bool,
    /// Count of consecutive health-check failures.
    pub consecutive_failures: u32,
    /// `true` when this model was adopted from an already-running process
    /// rather than spawned by us.
    pub adopted: bool,
    /// Timestamp of the most recent successful health check.
    #[serde(skip)]
    pub last_healthy_at: Option<Instant>,
}

// ─── Process Manager ─────────────────────────────────────────────────────────

/// Manages the full lifecycle of llama-server processes across multiple ports.
///
/// Thread-safe: inner state is behind `Arc<RwLock<..>>`.
#[derive(Clone)]
pub struct ProcessManager {
    models: Arc<RwLock<HashMap<u16, ManagedModel>>>,
    config: ProcessManagerConfig,
}

impl ProcessManager {
    /// Create a new process manager with default configuration.
    pub fn new() -> Self {
        Self {
            models: Arc::new(RwLock::new(HashMap::new())),
            config: ProcessManagerConfig::default(),
        }
    }

    /// Create with explicit configuration.
    pub fn with_config(config: ProcessManagerConfig) -> Self {
        Self {
            models: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    // ── Scan & Adopt ──────────────────────────────────────────────────────

    /// Detect running `llama-server` processes and adopt any on expected ports.
    ///
    /// `expected_ports` lists the ports we consider "ours". Processes on
    /// unexpected ports are returned but **not** adopted.
    pub async fn scan_and_adopt(&self, expected_ports: &[u16]) -> Result<Vec<DetectedProcess>> {
        let detected = detect_running_processes()?;
        let mut models = self.models.write().await;

        for proc in &detected {
            if let Some(port) = proc.port
                && expected_ports.contains(&port)
                && !models.contains_key(&port)
            {
                info!(
                    pid = proc.pid,
                    port,
                    model = proc.model_path.as_deref().unwrap_or("unknown"),
                    "adopting existing llama-server process"
                );

                models.insert(
                    port,
                    ManagedModel {
                        config: EngineConfig {
                            model_path: proc.model_path.as_deref().unwrap_or("").into(),
                            model_id: String::new(),
                            host: "0.0.0.0".into(),
                            port,
                            ctx_size: 8192,
                            gpu_layers: -1,
                            parallel: 4,
                            extra_args: Vec::new(),
                        },
                        pid: Some(proc.pid),
                        healthy: false, // will be confirmed on first health sweep
                        consecutive_failures: 0,
                        adopted: true,
                        last_healthy_at: None,
                    },
                );
            }
        }

        Ok(detected)
    }

    // ── Start / Stop ──────────────────────────────────────────────────────

    /// Start a model on a given port.
    ///
    /// Spawns `llama-server` with the supplied configuration and registers
    /// it in the managed map.
    pub async fn start_model(&self, config: EngineConfig) -> Result<u32> {
        let port = config.port;

        {
            let models = self.models.read().await;
            if let Some(existing) = models.get(&port)
                && existing.pid.is_some()
            {
                return Err(RuntimeError::AlreadyRunning { port });
            }
        }

        let binary = find_llama_server_binary()?;
        let args = build_llama_args(&config);

        info!(
            binary = %binary,
            port,
            model = %config.model_path.display(),
            "starting llama-server"
        );

        let child = Command::new(&binary)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| RuntimeError::StartFailed {
                reason: format!("failed to spawn llama-server: {e}"),
            })?;

        let pid = child.id();

        let mut models = self.models.write().await;
        models.insert(
            port,
            ManagedModel {
                config: config.clone(),
                pid: Some(pid),
                healthy: false,
                consecutive_failures: 0,
                adopted: false,
                last_healthy_at: None,
            },
        );

        info!(pid, port, "llama-server spawned");
        Ok(pid)
    }

    /// Stop a model on a given port.
    ///
    /// Sends `SIGTERM` first, waits up to `stop_timeout_secs`, then `SIGKILL`.
    pub async fn stop_model(&self, port: u16) -> Result<()> {
        let pid = {
            let models = self.models.read().await;
            match models.get(&port) {
                Some(m) => m.pid,
                None => return Err(RuntimeError::NotRunning),
            }
        };

        let pid = pid.ok_or(RuntimeError::NotRunning)?;

        info!(pid, port, "stopping llama-server");

        // SIGTERM
        send_signal(pid, "TERM");

        let start = Instant::now();
        let timeout = Duration::from_secs(self.config.stop_timeout_secs);

        loop {
            if !is_pid_alive(pid) {
                info!(pid, port, "llama-server stopped gracefully");
                break;
            }
            if start.elapsed() > timeout {
                warn!(pid, port, "SIGTERM timeout, sending SIGKILL");
                send_signal(pid, "KILL");
                // Give the kernel a moment
                tokio::time::sleep(Duration::from_millis(500)).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let mut models = self.models.write().await;
        if let Some(model) = models.get_mut(&port) {
            model.pid = None;
            model.healthy = false;
        }

        Ok(())
    }

    // ── Health Checks ─────────────────────────────────────────────────────

    /// Run a health check against a single port.
    ///
    /// Returns `true` if the server responded with HTTP 2xx on `/health`.
    pub async fn health_check(&self, port: u16) -> Result<bool> {
        let host = {
            let models = self.models.read().await;
            models
                .get(&port)
                .map(|m| m.config.host.clone())
                .unwrap_or_else(|| "127.0.0.1".into())
        };

        health_probe(&host, port, self.config.health_probe_timeout_secs).await
    }

    /// Run health checks on **all** managed models and update their state.
    ///
    /// Returns the number of models that are healthy.
    pub async fn health_check_all(&self) -> usize {
        let ports: Vec<u16> = {
            let models = self.models.read().await;
            models.keys().copied().collect()
        };

        let mut healthy_count = 0;

        for port in ports {
            let host = {
                let models = self.models.read().await;
                models
                    .get(&port)
                    .map(|m| m.config.host.clone())
                    .unwrap_or_else(|| "127.0.0.1".into())
            };

            let healthy = health_probe(&host, port, self.config.health_probe_timeout_secs)
                .await
                .unwrap_or(false);

            let mut models = self.models.write().await;
            if let Some(model) = models.get_mut(&port) {
                model.healthy = healthy;
                if healthy {
                    model.consecutive_failures = 0;
                    model.last_healthy_at = Some(Instant::now());
                    healthy_count += 1;
                } else {
                    model.consecutive_failures += 1;
                    debug!(
                        port,
                        failures = model.consecutive_failures,
                        "health check failed"
                    );
                }
            }
        }

        healthy_count
    }

    // ── Auto-Restart ──────────────────────────────────────────────────────

    /// Check all models and restart any that have exceeded
    /// `max_health_failures` consecutive failures.
    ///
    /// Returns a list of (port, new_pid) pairs for models that were restarted.
    pub async fn restart_crashed(&self) -> Vec<(u16, u32)> {
        let candidates: Vec<(u16, EngineConfig)> = {
            let models = self.models.read().await;
            models
                .iter()
                .filter(|(_, m)| m.consecutive_failures >= self.config.max_health_failures)
                .map(|(port, m)| (*port, m.config.clone()))
                .collect()
        };

        let mut restarted = Vec::new();

        for (port, config) in candidates {
            warn!(port, "model exceeded max health failures, restarting");

            // Kill existing process if still lingering
            let old_pid = {
                let models = self.models.read().await;
                models.get(&port).and_then(|m| m.pid)
            };

            if let Some(pid) = old_pid {
                send_signal(pid, "KILL");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            // Clear old entry
            {
                let mut models = self.models.write().await;
                models.remove(&port);
            }

            // Attempt restart
            match self.start_model(config).await {
                Ok(pid) => {
                    info!(port, pid, "model restarted successfully");
                    restarted.push((port, pid));
                }
                Err(err) => {
                    error!(port, error = %err, "failed to restart model");
                }
            }
        }

        restarted
    }

    // ── Status / Introspection ────────────────────────────────────────────

    /// Snapshot of all managed models keyed by port.
    pub async fn status(&self) -> HashMap<u16, ManagedModel> {
        self.models.read().await.clone()
    }

    /// Number of models currently managed.
    pub async fn model_count(&self) -> usize {
        self.models.read().await.len()
    }

    /// Remove a model entry (without stopping — call `stop_model` first).
    pub async fn remove_model(&self, port: u16) -> Option<ManagedModel> {
        self.models.write().await.remove(&port)
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Send a Unix signal to a process by PID.
#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
}

#[cfg(not(unix))]
fn send_signal(pid: u32, signal: &str) {
    let _ = (pid, signal); // no-op on non-Unix
}

/// Check if a PID is still alive.
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill -0` checks existence without actually signalling.
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Find the `llama-server` binary on PATH.
fn find_llama_server_binary() -> Result<String> {
    let candidates = [
        "llama-server",
        "/usr/local/bin/llama-server",
        "/opt/homebrew/bin/llama-server",
    ];

    for c in candidates {
        if Command::new("which")
            .arg(c)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Ok(c.to_string());
        }
    }

    Err(RuntimeError::BinaryNotFound {
        name: "llama-server".into(),
    })
}

/// Build command-line arguments for `llama-server`.
fn build_llama_args(config: &EngineConfig) -> Vec<String> {
    let mut args = vec![
        "--model".into(),
        config.model_path.display().to_string(),
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
        // -1 means "all layers"
        args.push("--n-gpu-layers".into());
        args.push("999".into());
    }

    args.extend(config.extra_args.iter().cloned());
    args
}

/// HTTP health probe: `GET http://{host}:{port}/health`.
async fn health_probe(host: &str, port: u16, timeout_secs: u64) -> Result<bool> {
    let url = format!("http://{host}:{port}/health");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| RuntimeError::HealthCheckFailed {
            reason: e.to_string(),
        })?;

    match client.get(&url).send().await {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_) => Ok(false),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_ps_output tests ─────────────────────────────────────────

    #[test]
    fn parse_ps_finds_llama_server_with_port_and_model() {
        let ps = "\
USER       PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND
root         1  0.0  0.0   2468  1460 ?        Ss   Mar31   0:02 /sbin/init
venkat   12345 45.2 12.1 9876543 654321 ?      Sl   10:00   5:32 /usr/local/bin/llama-server --model /models/qwen3-32b-q4.gguf --port 51800 --ctx-size 8192 --n-gpu-layers 999
venkat   67890  3.1  4.2 1234567 112233 ?      Sl   11:00   1:05 llama-server --model /models/qwen3-9b.gguf --port 51801 --parallel 4
";
        let procs = parse_ps_output(ps);
        assert_eq!(procs.len(), 2);

        assert_eq!(procs[0].pid, 12345);
        assert_eq!(procs[0].port, Some(51800));
        assert_eq!(
            procs[0].model_path.as_deref(),
            Some("/models/qwen3-32b-q4.gguf")
        );

        assert_eq!(procs[1].pid, 67890);
        assert_eq!(procs[1].port, Some(51801));
        assert_eq!(
            procs[1].model_path.as_deref(),
            Some("/models/qwen3-9b.gguf")
        );
    }

    #[test]
    fn parse_ps_ignores_grep_line() {
        let ps = "\
USER       PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND
venkat   99999  0.0  0.0   5000  1000 pts/0    S+   10:00   0:00 grep --color=auto llama-server
venkat   12345 10.0  5.0 9000000 500000 ?      Sl   10:00   2:00 llama-server --model /m/test.gguf --port 51800
";
        let procs = parse_ps_output(ps);
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 12345);
    }

    #[test]
    fn parse_ps_empty_output() {
        let procs = parse_ps_output("");
        assert!(procs.is_empty());
    }

    #[test]
    fn parse_ps_no_port_or_model() {
        let ps = "\
USER       PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND
venkat   11111  1.0  2.0  100000  50000 ?      Sl   10:00   0:30 llama-server
";
        let procs = parse_ps_output(ps);
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 11111);
        assert_eq!(procs[0].port, None);
        assert_eq!(procs[0].model_path, None);
    }

    #[test]
    fn parse_ps_detects_llama_underscore() {
        let ps = "\
USER       PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND
venkat   22222  5.0  3.0 200000 100000 ?       Sl   10:00   1:00 /opt/bin/llama_server --model /m/test.gguf --port 51802
";
        let procs = parse_ps_output(ps);
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 22222);
        assert_eq!(procs[0].port, Some(51802));
    }

    // ── extract_flag_value tests ──────────────────────────────────────

    #[test]
    fn flag_extraction_finds_long_flag() {
        let tokens = ["llama-server", "--model", "/m/test.gguf", "--port", "51800"];
        assert_eq!(
            extract_flag_value(&tokens, &["--port", "-p"]),
            Some("51800")
        );
        assert_eq!(
            extract_flag_value(&tokens, &["--model", "-m"]),
            Some("/m/test.gguf")
        );
    }

    #[test]
    fn flag_extraction_returns_none_for_missing() {
        let tokens = ["llama-server", "--port", "51800"];
        assert_eq!(extract_flag_value(&tokens, &["--model", "-m"]), None);
    }

    // ── ProcessManager unit tests ─────────────────────────────────────

    #[tokio::test]
    async fn process_manager_starts_empty() {
        let pm = ProcessManager::new();
        assert_eq!(pm.model_count().await, 0);
        assert!(pm.status().await.is_empty());
    }

    #[tokio::test]
    async fn process_manager_stop_nonexistent_returns_error() {
        let pm = ProcessManager::new();
        let result = pm.stop_model(51800).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn health_check_all_empty_returns_zero() {
        let pm = ProcessManager::new();
        let healthy = pm.health_check_all().await;
        assert_eq!(healthy, 0);
    }

    #[tokio::test]
    async fn restart_crashed_empty_returns_empty() {
        let pm = ProcessManager::new();
        let restarted = pm.restart_crashed().await;
        assert!(restarted.is_empty());
    }

    #[test]
    fn build_args_includes_all_flags() {
        let config = EngineConfig {
            model_path: "/models/test.gguf".into(),
            model_id: "test".into(),
            host: "0.0.0.0".into(),
            port: 51800,
            ctx_size: 16384,
            gpu_layers: -1,
            parallel: 8,
            extra_args: vec!["--flash-attn".into()],
        };
        let args = build_llama_args(&config);

        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"/models/test.gguf".to_string()));
        assert!(args.contains(&"--port".to_string()));
        assert!(args.contains(&"51800".to_string()));
        assert!(args.contains(&"--ctx-size".to_string()));
        assert!(args.contains(&"16384".to_string()));
        assert!(args.contains(&"999".to_string())); // gpu_layers = -1 → 999
        assert!(args.contains(&"--flash-attn".to_string()));
        assert!(args.contains(&"--parallel".to_string()));
        assert!(args.contains(&"8".to_string()));
    }

    #[test]
    fn build_args_explicit_gpu_layers() {
        let config = EngineConfig {
            model_path: "/models/test.gguf".into(),
            model_id: "test".into(),
            host: "0.0.0.0".into(),
            port: 51800,
            ctx_size: 8192,
            gpu_layers: 32,
            parallel: 4,
            extra_args: vec![],
        };
        let args = build_llama_args(&config);
        assert!(args.contains(&"32".to_string()));
        assert!(!args.contains(&"999".to_string()));
    }
}
