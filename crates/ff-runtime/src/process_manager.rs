//! Process manager for llama-server instances.
//!
//! Provides higher-level lifecycle management on top of [`LlamaCppEngine`]:
//!
//! - **Detection** — scan running `llama-server` processes via `ps aux`
//! - **Adoption** — claim existing processes on expected ports
//! - **Health monitoring** — periodic HTTP `/health` probes
//! - **Auto-restart** — restart crashed models after N consecutive failures
//! - **Start / Stop** — spawn or terminate `llama-server` with correct args

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::engine::EngineConfig;
use crate::error::{Result, RuntimeError};

/// Root-authoritative strict runtime policy. Callers cannot redirect this path.
/// Absence preserves legacy discovery; any present-but-invalid file fails shut.
pub const LLAMA_SERVER_RUNTIME_POLICY_PATH: &str = "/etc/forgefleet/llama-server-runtime.json";

const MAX_RUNTIME_POLICY_BYTES: u64 = 64 * 1024;

/// A content-addressed runtime artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedRuntimeArtifact {
    /// Exact, absolute, canonical path. Symlinks in any path component fail.
    pub path: PathBuf,
    /// Lower- or upper-case hexadecimal SHA-256 digest.
    pub sha256: String,
}

/// Fail-closed strict runtime policy. The `backend` tag deliberately has no
/// legacy variant: legacy behavior is available only by omitting the policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum LlamaServerRuntimePolicy {
    /// Immutable ROCm/HIP runtime (Logan's production profile).
    Rocm {
        binary_path: PathBuf,
        hip_library_path: PathBuf,
        /// Exact contents of the isolated runtime directory. Unknown sibling
        /// files are rejected so the dynamic backend loader cannot pick up an
        /// unverified library.
        bundle_artifacts: Vec<PinnedRuntimeArtifact>,
        /// Exact canonical files resolved outside the bundle by `ldd` for the
        /// executable and HIP backend (ROCm, libc, libstdc++, and peers).
        loader_dependencies: Vec<PinnedRuntimeArtifact>,
        target_os: String,
        target_arch: String,
        os_id: String,
        os_version_id: String,
        rocm_version: String,
        gpu_arch: String,
    },
    /// Deliberate rollback to a separately pinned CPU-only runtime.
    CpuRollback {
        /// Presence of this variant in the fixed root-owned policy is the
        /// authorization; there is no caller-controlled boolean bypass.
        binary_path: PathBuf,
        bundle_artifacts: Vec<PinnedRuntimeArtifact>,
        loader_dependencies: Vec<PinnedRuntimeArtifact>,
        target_os: String,
        target_arch: String,
        os_id: String,
        os_version_id: String,
    },
}

/// Backend identity returned only after runtime verification succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlamaServerBackend {
    Legacy,
    Rocm,
    CpuRollback,
}

/// Verified executable selection consumed by both inference-server spawners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaServerSelection {
    pub path: PathBuf,
    pub backend: LlamaServerBackend,
    /// Strict policies use only this directory for co-located runtime libs.
    strict_library_dir: Option<PathBuf>,
    verified_artifacts: Vec<VerifiedRuntimeArtifact>,
    authority_uid: u32,
    protect_ancestors: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedRuntimeArtifact {
    path: PathBuf,
    sha256: String,
    device: u64,
    inode: u64,
    len: u64,
    executable: bool,
}

impl LlamaServerSelection {
    pub fn is_strict(&self) -> bool {
        self.backend != LlamaServerBackend::Legacy
    }

    pub fn strict_library_dir(&self) -> Option<&Path> {
        self.strict_library_dir.as_deref()
    }

    /// Apply loader protections to a strict launch. The executable remains an
    /// exact absolute path, while inherited loader injection is removed.
    pub fn configure_command(&self, command: &mut Command) {
        if let Some(dir) = &self.strict_library_dir {
            command
                .env_remove("LD_PRELOAD")
                .env_remove("LD_AUDIT")
                .env("LD_LIBRARY_PATH", dir);
        }
    }

    /// Re-open every pinned file without following a final symlink, compare
    /// inode identity, ownership/mode/link count, and hash again immediately
    /// before spawn. The root-owned non-writable ancestor chain prevents an
    /// unprivileged replacement between this check and `execve`.
    pub fn revalidate(&self) -> std::result::Result<(), LlamaRuntimePolicyError> {
        for verified in &self.verified_artifacts {
            let current = validate_pinned_artifact(
                &PinnedRuntimeArtifact {
                    path: verified.path.clone(),
                    sha256: verified.sha256.clone(),
                },
                verified.executable,
                self.authority_uid,
                self.protect_ancestors,
            )?;
            if current.device != verified.device
                || current.inode != verified.inode
                || current.len != verified.len
            {
                return Err(LlamaRuntimePolicyError::Artifact {
                    path: verified.path.clone(),
                    reason: "inode identity changed after attestation".into(),
                });
            }
        }
        Ok(())
    }

    /// Confirm that the process which actually crossed `execve` still has the
    /// pinned executable and strict loader environment. This catches a changed
    /// systemd `ExecStart`/environment as well as an accidental spawn-path
    /// divergence before the endpoint can be activated.
    pub fn attest_process(&self, pid: u32) -> std::result::Result<(), LlamaRuntimePolicyError> {
        if !self.is_strict() {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            self.revalidate()?;
            let proc_root = PathBuf::from(format!("/proc/{pid}"));
            let executable = std::fs::read_link(proc_root.join("exe")).map_err(|e| {
                LlamaRuntimePolicyError::Artifact {
                    path: proc_root.join("exe"),
                    reason: e.to_string(),
                }
            })?;
            if executable != self.path {
                return Err(LlamaRuntimePolicyError::Artifact {
                    path: executable,
                    reason: format!(
                        "launched executable differs from pinned {}",
                        self.path.display()
                    ),
                });
            }
            let environ = std::fs::read(proc_root.join("environ")).map_err(|e| {
                LlamaRuntimePolicyError::Artifact {
                    path: proc_root.join("environ"),
                    reason: e.to_string(),
                }
            })?;
            attest_loader_environment(
                &environ,
                self.strict_library_dir
                    .as_deref()
                    .expect("strict selection has library directory"),
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            Err(LlamaRuntimePolicyError::Config(
                "strict process identity attestation requires Linux /proc".into(),
            ))
        }
    }
}

/// Deterministic snapshot used to verify platform and HIP/backend identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlamaRuntimeEvidence {
    pub target_os: String,
    pub target_arch: String,
    pub os_id: String,
    pub os_version_id: String,
    pub rocm_version: Option<String>,
    pub gpu_arches: Vec<String>,
    pub hip_ldd: Option<String>,
    pub binary_ldd: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlamaRuntimePolicyError {
    #[error("runtime policy configuration error: {0}")]
    Config(String),
    #[error("runtime artifact `{path}` is invalid: {reason}")]
    Artifact { path: PathBuf, reason: String },
    #[error("runtime artifact hash mismatch for `{path}`: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("runtime platform mismatch for {field}: expected `{expected}`, got `{actual}`")]
    PlatformMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("ROCm/HIP backend attestation failed: {0}")]
    HipAttestation(String),
    #[error("legacy llama-server binary not found")]
    LegacyBinaryNotFound,
}

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

        let selection = resolve_llama_server_binary()
            .map_err(|error| RuntimeError::Other(error.to_string()))?;
        validate_strict_gpu_overrides(&config, selection.backend).map_err(RuntimeError::Other)?;
        let binary = selection.path.display().to_string();
        let args = build_llama_args(&config, selection.backend);

        info!(
            binary = %binary,
            port,
            model = %config.model_path.display(),
            "starting llama-server"
        );

        let mut command = Command::new(&binary);
        command
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        selection.configure_command(&mut command);
        selection
            .revalidate()
            .map_err(|error| RuntimeError::Other(error.to_string()))?;
        let mut child = command.spawn().map_err(|e| RuntimeError::StartFailed {
            reason: format!("failed to spawn llama-server: {e}"),
        })?;

        let pid = child.id();
        if let Err(error) = selection.attest_process(pid) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RuntimeError::StartFailed {
                reason: format!("post-spawn runtime attestation failed: {error}"),
            });
        }

        // Reap the child in a blocking thread so it doesn't become a zombie.
        tokio::task::spawn_blocking(move || {
            let _ = child.wait();
        });

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
pub(crate) fn send_signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
}

#[cfg(not(unix))]
pub(crate) fn send_signal(pid: u32, signal: &str) {
    let _ = (pid, signal); // no-op on non-Unix
}

/// Check if a PID is still alive.
pub(crate) fn is_pid_alive(pid: u32) -> bool {
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

/// Resolve and attest the llama-server runtime for this launch.
///
/// A configured policy is fail-closed. Only a completely absent policy keeps
/// the historical fleet-wide discovery behavior.
pub fn resolve_llama_server_binary()
-> std::result::Result<LlamaServerSelection, LlamaRuntimePolicyError> {
    let policy_path = Path::new(LLAMA_SERVER_RUNTIME_POLICY_PATH);
    match std::fs::symlink_metadata(policy_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return resolve_legacy_llama_server();
        }
        Err(error) => {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: policy_path.to_path_buf(),
                reason: error.to_string(),
            });
        }
    }
    let policy = load_llama_runtime_policy(policy_path, 0, true)?;
    // Hash before invoking any evidence helper, then hash again as part of
    // verification immediately before handing the path to the spawner.
    validate_policy_artifacts(&policy, 0, true)?;
    let evidence = collect_runtime_evidence(&policy)?;
    verify_llama_runtime_policy(&policy, &evidence)
}

/// Verify a strict policy against an injected evidence snapshot. File bytes
/// are always read from the pinned paths; hardware/command evidence is passed
/// in so mismatch behavior has deterministic tests.
pub fn verify_llama_runtime_policy(
    policy: &LlamaServerRuntimePolicy,
    evidence: &LlamaRuntimeEvidence,
) -> std::result::Result<LlamaServerSelection, LlamaRuntimePolicyError> {
    verify_llama_runtime_policy_for_authority(policy, evidence, 0, true)
}

fn verify_llama_runtime_policy_for_authority(
    policy: &LlamaServerRuntimePolicy,
    evidence: &LlamaRuntimeEvidence,
    authority_uid: u32,
    protect_ancestors: bool,
) -> std::result::Result<LlamaServerSelection, LlamaRuntimePolicyError> {
    match policy {
        LlamaServerRuntimePolicy::Rocm {
            binary_path,
            hip_library_path,
            bundle_artifacts,
            loader_dependencies,
            target_os,
            target_arch,
            os_id,
            os_version_id,
            rocm_version,
            gpu_arch,
        } => {
            validate_platform(evidence, target_os, target_arch, os_id, os_version_id)?;
            let (binary_dir, verified_artifacts) = validate_runtime_manifest(
                binary_path,
                Some(hip_library_path),
                bundle_artifacts,
                loader_dependencies,
                authority_uid,
                protect_ancestors,
            )?;

            require_exact(
                "ROCm version",
                rocm_version,
                evidence.rocm_version.as_deref().unwrap_or("<missing>"),
            )?;
            if evidence.gpu_arches.is_empty()
                || evidence.gpu_arches.iter().any(|arch| arch != gpu_arch)
            {
                return Err(LlamaRuntimePolicyError::PlatformMismatch {
                    field: "GPU architecture set",
                    expected: gpu_arch.clone(),
                    actual: if evidence.gpu_arches.is_empty() {
                        "<missing>".into()
                    } else {
                        evidence.gpu_arches.join(",")
                    },
                });
            }

            let ldd = evidence.hip_ldd.as_deref().ok_or_else(|| {
                LlamaRuntimePolicyError::HipAttestation(
                    "missing libggml-hip loader evidence".into(),
                )
            })?;
            attest_hip_linkage(ldd)?;
            let binary_ldd = evidence.binary_ldd.as_deref().ok_or_else(|| {
                LlamaRuntimePolicyError::HipAttestation(
                    "missing llama-server loader evidence".into(),
                )
            })?;
            attest_loader_manifest(&[binary_ldd, ldd], bundle_artifacts, loader_dependencies)?;

            Ok(LlamaServerSelection {
                path: binary_path.clone(),
                backend: LlamaServerBackend::Rocm,
                strict_library_dir: Some(binary_dir),
                verified_artifacts,
                authority_uid,
                protect_ancestors,
            })
        }
        LlamaServerRuntimePolicy::CpuRollback {
            binary_path,
            bundle_artifacts,
            loader_dependencies,
            target_os,
            target_arch,
            os_id,
            os_version_id,
        } => {
            validate_platform(evidence, target_os, target_arch, os_id, os_version_id)?;
            let (binary_dir, verified_artifacts) = validate_runtime_manifest(
                binary_path,
                None,
                bundle_artifacts,
                loader_dependencies,
                authority_uid,
                protect_ancestors,
            )?;
            let ldd = evidence.binary_ldd.as_deref().ok_or_else(|| {
                LlamaRuntimePolicyError::Config(
                    "missing CPU rollback binary loader evidence".into(),
                )
            })?;
            let lower = ldd.to_ascii_lowercase();
            if lower.contains("libamdhip64") || lower.contains("libggml-hip") {
                return Err(LlamaRuntimePolicyError::HipAttestation(
                    "CPU rollback binary has ROCm/HIP linkage".into(),
                ));
            }
            reject_hip_backend_in_directory(&binary_dir)?;
            attest_loader_manifest(&[ldd], bundle_artifacts, loader_dependencies)?;

            Ok(LlamaServerSelection {
                path: binary_path.clone(),
                backend: LlamaServerBackend::CpuRollback,
                strict_library_dir: Some(binary_dir),
                verified_artifacts,
                authority_uid,
                protect_ancestors,
            })
        }
    }
}

fn load_llama_runtime_policy(
    path: &Path,
    authority_uid: u32,
    protect_ancestors: bool,
) -> std::result::Result<LlamaServerRuntimePolicy, LlamaRuntimePolicyError> {
    let (mut file, metadata) =
        open_authoritative_file(path, false, authority_uid, protect_ancestors)?;
    if metadata.len() > MAX_RUNTIME_POLICY_BYTES {
        return Err(LlamaRuntimePolicyError::Config(format!(
            "policy file exceeds {MAX_RUNTIME_POLICY_BYTES} bytes"
        )));
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
    serde_json::from_str(&raw)
        .map_err(|e| LlamaRuntimePolicyError::Config(format!("invalid policy JSON: {e}")))
}

fn validate_policy_artifacts(
    policy: &LlamaServerRuntimePolicy,
    authority_uid: u32,
    protect_ancestors: bool,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    match policy {
        LlamaServerRuntimePolicy::Rocm {
            binary_path,
            hip_library_path,
            bundle_artifacts,
            loader_dependencies,
            ..
        } => {
            validate_runtime_manifest(
                binary_path,
                Some(hip_library_path),
                bundle_artifacts,
                loader_dependencies,
                authority_uid,
                protect_ancestors,
            )?;
        }
        LlamaServerRuntimePolicy::CpuRollback {
            binary_path,
            bundle_artifacts,
            loader_dependencies,
            ..
        } => {
            validate_runtime_manifest(
                binary_path,
                None,
                bundle_artifacts,
                loader_dependencies,
                authority_uid,
                protect_ancestors,
            )?;
        }
    }
    Ok(())
}

fn validate_pinned_artifact(
    artifact: &PinnedRuntimeArtifact,
    executable: bool,
    authority_uid: u32,
    protect_ancestors: bool,
) -> std::result::Result<VerifiedRuntimeArtifact, LlamaRuntimePolicyError> {
    let expected = normalize_sha256(&artifact.sha256).ok_or_else(|| {
        LlamaRuntimePolicyError::Config(format!(
            "invalid SHA-256 for {} (expected 64 hexadecimal characters)",
            artifact.path.display()
        ))
    })?;
    let (mut file, before) =
        open_authoritative_file(&artifact.path, executable, authority_uid, protect_ancestors)?;
    let actual = sha256_reader(&artifact.path, &mut file)?;
    if actual != expected {
        return Err(LlamaRuntimePolicyError::HashMismatch {
            path: artifact.path.clone(),
            expected,
            actual,
        });
    }
    let after = file
        .metadata()
        .map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: artifact.path.clone(),
            reason: e.to_string(),
        })?;
    let before_identity = file_identity(&before);
    let after_identity = file_identity(&after);
    if before_identity != after_identity || before.len() != after.len() {
        return Err(LlamaRuntimePolicyError::Artifact {
            path: artifact.path.clone(),
            reason: "file identity changed while hashing".into(),
        });
    }
    Ok(VerifiedRuntimeArtifact {
        path: artifact.path.clone(),
        sha256: expected,
        device: before_identity.0,
        inode: before_identity.1,
        len: before.len(),
        executable,
    })
}

fn open_authoritative_file(
    path: &Path,
    executable: bool,
    authority_uid: u32,
    protect_ancestors: bool,
) -> std::result::Result<(File, std::fs::Metadata), LlamaRuntimePolicyError> {
    if !path.is_absolute() {
        return Err(LlamaRuntimePolicyError::Artifact {
            path: path.to_path_buf(),
            reason: "path is not absolute".into(),
        });
    }
    let canonical = std::fs::canonicalize(path).map_err(|e| LlamaRuntimePolicyError::Artifact {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if canonical != path {
        return Err(LlamaRuntimePolicyError::Artifact {
            path: path.to_path_buf(),
            reason: format!(
                "path is not canonical (resolved to {})",
                canonical.display()
            ),
        });
    }

    if protect_ancestors {
        validate_authoritative_ancestors(path, authority_uid)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
    let metadata = file
        .metadata()
        .map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
    if !metadata.is_file() {
        return Err(LlamaRuntimePolicyError::Artifact {
            path: path.to_path_buf(),
            reason: "path is not a regular file".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != authority_uid {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: path.to_path_buf(),
                reason: format!(
                    "owner uid {} does not match authority uid {authority_uid}",
                    metadata.uid()
                ),
            });
        }
        if metadata.nlink() != 1 {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: path.to_path_buf(),
                reason: format!("link count {} is not 1", metadata.nlink()),
            });
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: path.to_path_buf(),
                reason: "file is group- or world-writable".into(),
            });
        }
        if metadata.permissions().mode() & 0o111 == 0 && executable {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: path.to_path_buf(),
                reason: "regular file is not executable".into(),
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (executable, authority_uid);
        return Err(LlamaRuntimePolicyError::Config(
            "strict llama-server policies require Unix ownership semantics".into(),
        ));
    }
    Ok((file, metadata))
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn validate_authoritative_ancestors(
    path: &Path,
    authority_uid: u32,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    let mut current = path.parent();
    while let Some(directory) = current {
        let metadata = std::fs::symlink_metadata(directory).map_err(|e| {
            LlamaRuntimePolicyError::Artifact {
                path: directory.to_path_buf(),
                reason: e.to_string(),
            }
        })?;
        if !metadata.is_dir() {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: directory.to_path_buf(),
                reason: "ancestor is not a directory".into(),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != authority_uid || metadata.permissions().mode() & 0o022 != 0 {
                return Err(LlamaRuntimePolicyError::Artifact {
                    path: directory.to_path_buf(),
                    reason: "ancestor is not authority-owned and non-writable by group/other"
                        .into(),
                });
            }
        }
        current = directory.parent();
    }
    Ok(())
}

fn normalize_sha256(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (trimmed.len() == 64 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| trimmed.to_ascii_lowercase())
}

fn sha256_reader(
    path: &Path,
    file: &mut File,
) -> std::result::Result<String, LlamaRuntimePolicyError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|e| LlamaRuntimePolicyError::Artifact {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_runtime_manifest(
    binary_path: &Path,
    hip_library_path: Option<&PathBuf>,
    bundle_artifacts: &[PinnedRuntimeArtifact],
    loader_dependencies: &[PinnedRuntimeArtifact],
    authority_uid: u32,
    protect_ancestors: bool,
) -> std::result::Result<(PathBuf, Vec<VerifiedRuntimeArtifact>), LlamaRuntimePolicyError> {
    let bundle_dir = binary_path
        .parent()
        .ok_or_else(|| LlamaRuntimePolicyError::Artifact {
            path: binary_path.to_path_buf(),
            reason: "binary has no parent directory".into(),
        })?
        .to_path_buf();
    if hip_library_path.is_some_and(|path| path.parent() != Some(bundle_dir.as_path())) {
        return Err(LlamaRuntimePolicyError::HipAttestation(
            "llama-server and libggml-hip must be co-located".into(),
        ));
    }
    let mut manifest = BTreeMap::new();
    for artifact in bundle_artifacts {
        if artifact.path.parent() != Some(bundle_dir.as_path()) {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: artifact.path.clone(),
                reason: "bundle artifact is outside the isolated runtime directory".into(),
            });
        }
        if manifest.insert(artifact.path.clone(), artifact).is_some() {
            return Err(LlamaRuntimePolicyError::Config(format!(
                "duplicate bundle artifact {}",
                artifact.path.display()
            )));
        }
    }
    if !manifest.contains_key(binary_path) {
        return Err(LlamaRuntimePolicyError::Config(
            "binary_path is absent from bundle_artifacts".into(),
        ));
    }
    if let Some(hip_path) = hip_library_path
        && !manifest.contains_key(hip_path)
    {
        return Err(LlamaRuntimePolicyError::Config(
            "hip_library_path is absent from bundle_artifacts".into(),
        ));
    }
    validate_exact_bundle_contents(&bundle_dir, manifest.keys(), authority_uid)?;

    let mut verified = Vec::new();
    for artifact in bundle_artifacts {
        verified.push(validate_pinned_artifact(
            artifact,
            artifact.path == binary_path,
            authority_uid,
            protect_ancestors,
        )?);
    }
    let mut dependency_paths = BTreeSet::new();
    for dependency in loader_dependencies {
        if !dependency_paths.insert(dependency.path.clone()) {
            return Err(LlamaRuntimePolicyError::Config(format!(
                "duplicate loader dependency {}",
                dependency.path.display()
            )));
        }
        verified.push(validate_pinned_artifact(
            dependency,
            false,
            authority_uid,
            protect_ancestors,
        )?);
    }
    Ok((bundle_dir, verified))
}

fn validate_exact_bundle_contents<'a>(
    bundle_dir: &Path,
    expected: impl Iterator<Item = &'a PathBuf>,
    authority_uid: u32,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    validate_authoritative_directory(bundle_dir, authority_uid)?;
    let expected: BTreeSet<PathBuf> = expected.cloned().collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(bundle_dir).map_err(|e| LlamaRuntimePolicyError::Artifact {
        path: bundle_dir.to_path_buf(),
        reason: e.to_string(),
    })? {
        let entry = entry.map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: bundle_dir.to_path_buf(),
            reason: e.to_string(),
        })?;
        if !entry
            .file_type()
            .map_err(|e| LlamaRuntimePolicyError::Artifact {
                path: entry.path(),
                reason: e.to_string(),
            })?
            .is_file()
        {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: entry.path(),
                reason: "isolated runtime bundle contains a non-regular entry".into(),
            });
        }
        actual.insert(entry.path());
    }
    if actual != expected {
        return Err(LlamaRuntimePolicyError::Config(format!(
            "runtime bundle contents differ from manifest; expected {expected:?}, got {actual:?}"
        )));
    }
    Ok(())
}

fn validate_authoritative_directory(
    directory: &Path,
    authority_uid: u32,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    let metadata =
        std::fs::symlink_metadata(directory).map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: directory.to_path_buf(),
            reason: e.to_string(),
        })?;
    if !metadata.is_dir() {
        return Err(LlamaRuntimePolicyError::Artifact {
            path: directory.to_path_buf(),
            reason: "runtime bundle is not a directory".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != authority_uid || metadata.permissions().mode() & 0o022 != 0 {
            return Err(LlamaRuntimePolicyError::Artifact {
                path: directory.to_path_buf(),
                reason: "runtime bundle is not authority-owned and non-writable by group/other"
                    .into(),
            });
        }
    }
    Ok(())
}

fn validate_platform(
    evidence: &LlamaRuntimeEvidence,
    target_os: &str,
    target_arch: &str,
    os_id: &str,
    os_version_id: &str,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    require_exact("target OS", target_os, &evidence.target_os)?;
    require_exact("target architecture", target_arch, &evidence.target_arch)?;
    require_exact("OS ID", os_id, &evidence.os_id)?;
    require_exact("OS VERSION_ID", os_version_id, &evidence.os_version_id)
}

fn require_exact(
    field: &'static str,
    expected: &str,
    actual: &str,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    if expected.is_empty() || expected != actual {
        return Err(LlamaRuntimePolicyError::PlatformMismatch {
            field,
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

fn attest_hip_linkage(ldd: &str) -> std::result::Result<(), LlamaRuntimePolicyError> {
    let lower = ldd.to_ascii_lowercase();
    if lower.contains("not found") {
        return Err(LlamaRuntimePolicyError::HipAttestation(
            "libggml-hip has an unresolved dynamic dependency".into(),
        ));
    }
    let linked = ldd.lines().any(|line| {
        let line = line.trim();
        line.starts_with("libamdhip64.so")
            && line
                .split_once("=>")
                .is_some_and(|(_, resolved)| resolved.trim_start().starts_with('/'))
    });
    if !linked {
        return Err(LlamaRuntimePolicyError::HipAttestation(
            "libggml-hip does not resolve libamdhip64 from an absolute path".into(),
        ));
    }
    Ok(())
}

fn attest_loader_manifest(
    ldd_outputs: &[&str],
    bundle_artifacts: &[PinnedRuntimeArtifact],
    loader_dependencies: &[PinnedRuntimeArtifact],
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    let bundle: BTreeSet<PathBuf> = bundle_artifacts
        .iter()
        .map(|artifact| artifact.path.clone())
        .collect();
    let expected_external: BTreeSet<PathBuf> = loader_dependencies
        .iter()
        .map(|artifact| artifact.path.clone())
        .collect();
    let mut observed_external = BTreeSet::new();

    for output in ldd_outputs {
        if output.to_ascii_lowercase().contains("not found") {
            return Err(LlamaRuntimePolicyError::HipAttestation(
                "loader evidence contains an unresolved dependency".into(),
            ));
        }
        for path in resolved_ldd_paths(output)? {
            if !bundle.contains(&path) {
                observed_external.insert(path);
            }
        }
    }
    if observed_external != expected_external {
        return Err(LlamaRuntimePolicyError::HipAttestation(format!(
            "external loader dependency set differs from manifest; expected {expected_external:?}, got {observed_external:?}"
        )));
    }
    Ok(())
}

fn resolved_ldd_paths(
    output: &str,
) -> std::result::Result<BTreeSet<PathBuf>, LlamaRuntimePolicyError> {
    let mut paths = BTreeSet::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let candidate = if let Some((_, resolved)) = trimmed.split_once("=>") {
            resolved.split_whitespace().next()
        } else {
            trimmed.split_whitespace().next()
        };
        let Some(candidate) = candidate.filter(|value| value.starts_with('/')) else {
            continue;
        };
        let canonical = std::fs::canonicalize(candidate).map_err(|e| {
            LlamaRuntimePolicyError::HipAttestation(format!(
                "cannot canonicalize loader dependency {candidate}: {e}"
            ))
        })?;
        paths.insert(canonical);
    }
    Ok(paths)
}

fn reject_hip_backend_in_directory(
    directory: &Path,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    let entries = std::fs::read_dir(directory).map_err(|e| LlamaRuntimePolicyError::Artifact {
        path: directory.to_path_buf(),
        reason: e.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| LlamaRuntimePolicyError::Artifact {
            path: directory.to_path_buf(),
            reason: e.to_string(),
        })?;
        if entry
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("libggml-hip.so")
        {
            return Err(LlamaRuntimePolicyError::HipAttestation(format!(
                "CPU rollback directory contains HIP backend {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn attest_loader_environment(
    environ: &[u8],
    expected_library_dir: &Path,
) -> std::result::Result<(), LlamaRuntimePolicyError> {
    let mut ld_library_path = None;
    for field in environ
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        let Some(separator) = field.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (name, value_with_separator) = field.split_at(separator);
        let value = &value_with_separator[1..];
        if name == b"LD_PRELOAD" || name == b"LD_AUDIT" {
            return Err(LlamaRuntimePolicyError::HipAttestation(format!(
                "strict process inherited forbidden {}",
                String::from_utf8_lossy(name)
            )));
        }
        if name == b"LD_LIBRARY_PATH" {
            ld_library_path = Some(value);
        }
    }
    let expected = expected_library_dir.as_os_str().as_encoded_bytes();
    match ld_library_path {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(LlamaRuntimePolicyError::HipAttestation(format!(
            "strict LD_LIBRARY_PATH mismatch: expected {}, got {}",
            expected_library_dir.display(),
            String::from_utf8_lossy(actual)
        ))),
        None => Err(LlamaRuntimePolicyError::HipAttestation(
            "strict process is missing LD_LIBRARY_PATH".into(),
        )),
    }
}

fn collect_runtime_evidence(
    policy: &LlamaServerRuntimePolicy,
) -> std::result::Result<LlamaRuntimeEvidence, LlamaRuntimePolicyError> {
    let os_release = std::fs::read_to_string("/etc/os-release").map_err(|e| {
        LlamaRuntimePolicyError::Config(format!("cannot read /etc/os-release: {e}"))
    })?;
    let mut evidence = LlamaRuntimeEvidence {
        target_os: std::env::consts::OS.into(),
        target_arch: std::env::consts::ARCH.into(),
        os_id: os_release_value(&os_release, "ID").unwrap_or_default(),
        os_version_id: os_release_value(&os_release, "VERSION_ID").unwrap_or_default(),
        ..LlamaRuntimeEvidence::default()
    };

    match policy {
        LlamaServerRuntimePolicy::Rocm {
            binary_path,
            hip_library_path,
            ..
        } => {
            let library_dir =
                binary_path
                    .parent()
                    .ok_or_else(|| LlamaRuntimePolicyError::Artifact {
                        path: binary_path.clone(),
                        reason: "binary has no parent directory".into(),
                    })?;
            evidence.hip_ldd = Some(run_evidence_command(
                find_evidence_tool(&["/usr/bin/ldd"])?,
                &[hip_library_path.as_os_str()],
                Some(library_dir),
            )?);
            evidence.binary_ldd = Some(run_evidence_command(
                find_evidence_tool(&["/usr/bin/ldd"])?,
                &[binary_path.as_os_str()],
                Some(library_dir),
            )?);
            let hipconfig = run_evidence_command(
                find_evidence_tool(&["/usr/bin/hipconfig", "/opt/rocm/bin/hipconfig"])?,
                &[std::ffi::OsStr::new("--version")],
                None,
            )?;
            evidence.rocm_version = parse_hip_version(&hipconfig);
            if evidence.rocm_version.is_none() {
                return Err(LlamaRuntimePolicyError::HipAttestation(
                    "hipconfig --version did not report an exact HIP version".into(),
                ));
            }
            let rocminfo = run_evidence_command(
                find_evidence_tool(&["/usr/bin/rocminfo", "/opt/rocm/bin/rocminfo"])?,
                &[],
                None,
            )?;
            evidence.gpu_arches = parse_gfx_names(&rocminfo);
        }
        LlamaServerRuntimePolicy::CpuRollback { binary_path, .. } => {
            evidence.binary_ldd = Some(run_evidence_command(
                find_evidence_tool(&["/usr/bin/ldd"])?,
                &[binary_path.as_os_str()],
                binary_path.parent(),
            )?);
        }
    }
    Ok(evidence)
}

fn find_evidence_tool(
    candidates: &[&str],
) -> std::result::Result<PathBuf, LlamaRuntimePolicyError> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            LlamaRuntimePolicyError::Config(format!(
                "required evidence tool not found: {}",
                candidates.join(" or ")
            ))
        })
}

fn run_evidence_command(
    program: PathBuf,
    args: &[&std::ffi::OsStr],
    library_dir: Option<&Path>,
) -> std::result::Result<String, LlamaRuntimePolicyError> {
    let mut command = Command::new(&program);
    command
        .args(args)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_AUDIT");
    if let Some(dir) = library_dir {
        command.env("LD_LIBRARY_PATH", dir);
    } else {
        command.env_remove("LD_LIBRARY_PATH");
    }
    let output = command.output().map_err(|e| {
        LlamaRuntimePolicyError::HipAttestation(format!("failed to run {}: {e}", program.display()))
    })?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(LlamaRuntimePolicyError::HipAttestation(format!(
            "{} exited {}: {}",
            program.display(),
            output.status,
            text.trim()
        )));
    }
    Ok(text)
}

fn os_release_value(raw: &str, key: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let (found, value) = line.split_once('=')?;
        (found == key).then(|| value.trim().trim_matches('"').to_string())
    })
}

fn parse_hip_version(raw: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let (label, version) = line.trim().split_once(':')?;
        label
            .trim()
            .eq_ignore_ascii_case("HIP version")
            .then(|| version.trim().to_string())
            .filter(|version| !version.is_empty())
    })
}

fn parse_gfx_names(raw: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in raw.lines() {
        let Some((label, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim();
        if label.trim() == "Name"
            && value.starts_with("gfx")
            && value[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
            && !names.iter().any(|known| known == value)
        {
            names.push(value.to_string());
        }
    }
    names
}

fn resolve_legacy_llama_server()
-> std::result::Result<LlamaServerSelection, LlamaRuntimePolicyError> {
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for relative in [
            "llama.cpp/build/bin/llama-server",
            "projects/llama.cpp/build/bin/llama-server",
            ".forgefleet/llama.cpp/build/bin/llama-server",
        ] {
            candidates.push(home.join(relative));
        }
    }
    candidates.extend([
        PathBuf::from("/usr/local/bin/llama-server"),
        PathBuf::from("/opt/homebrew/bin/llama-server"),
    ]);
    if let Some(path) = std::env::var_os("PATH") {
        candidates
            .extend(std::env::split_paths(&path).map(|directory| directory.join("llama-server")));
    }
    let path = candidates
        .into_iter()
        .find(|candidate| is_legacy_executable(candidate))
        .ok_or(LlamaRuntimePolicyError::LegacyBinaryNotFound)?;
    Ok(LlamaServerSelection {
        path,
        backend: LlamaServerBackend::Legacy,
        strict_library_dir: None,
        verified_artifacts: Vec::new(),
        authority_uid: 0,
        protect_ancestors: true,
    })
}

fn is_legacy_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Build command-line arguments for `llama-server`.
fn build_llama_args(config: &EngineConfig, backend: LlamaServerBackend) -> Vec<String> {
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

    args.push("--n-gpu-layers".into());
    args.push(match backend {
        // Strict backend identity owns this setting; caller extras cannot
        // silently turn ROCm off or CPU rollback back on.
        LlamaServerBackend::Rocm => "999".into(),
        LlamaServerBackend::CpuRollback => "0".into(),
        LlamaServerBackend::Legacy if config.gpu_layers >= 0 => config.gpu_layers.to_string(),
        LlamaServerBackend::Legacy => "999".into(),
    });

    args.extend(config.extra_args.iter().cloned());
    args
}

fn validate_strict_gpu_overrides(
    config: &EngineConfig,
    backend: LlamaServerBackend,
) -> std::result::Result<(), String> {
    if backend == LlamaServerBackend::Legacy {
        return Ok(());
    }
    if config.extra_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--n-gpu-layers" | "-ngl" | "--device" | "--split-mode"
        )
    }) {
        return Err(format!(
            "strict {backend:?} runtime rejects caller GPU/backend override arguments"
        ));
    }
    Ok(())
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

    #[cfg(unix)]
    struct StrictFixture {
        _temp: tempfile::TempDir,
        policy: LlamaServerRuntimePolicy,
        evidence: LlamaRuntimeEvidence,
        binary: PathBuf,
        bundle_dir: PathBuf,
        authority_uid: u32,
    }

    #[cfg(unix)]
    fn write_artifact(path: &Path, bytes: &[u8], executable: bool) -> PinnedRuntimeArtifact {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, bytes).unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        let mut file = File::open(path).unwrap();
        PinnedRuntimeArtifact {
            path: path.to_path_buf(),
            sha256: sha256_reader(path, &mut file).unwrap(),
        }
    }

    #[cfg(unix)]
    fn strict_rocm_fixture() -> StrictFixture {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let bundle_dir = temp.path().join("runtime");
        let dependency_dir = temp.path().join("deps");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::create_dir_all(&dependency_dir).unwrap();
        std::fs::set_permissions(&bundle_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let binary = bundle_dir.join("llama-server");
        let hip = bundle_dir.join("libggml-hip.so");
        let base = bundle_dir.join("libggml-base.so");
        let amdhip = dependency_dir.join("libamdhip64.so.7");
        let libc = dependency_dir.join("libc.so.6");
        let binary_artifact = write_artifact(&binary, b"pinned llama server", true);
        let hip_artifact = write_artifact(&hip, b"pinned HIP backend", false);
        let base_artifact = write_artifact(&base, b"pinned ggml base", false);
        let amdhip_artifact = write_artifact(&amdhip, b"pinned amdhip", false);
        let libc_artifact = write_artifact(&libc, b"pinned libc", false);
        let authority_uid = std::fs::metadata(&binary).unwrap().uid();

        let policy = LlamaServerRuntimePolicy::Rocm {
            binary_path: binary.clone(),
            hip_library_path: hip.clone(),
            bundle_artifacts: vec![binary_artifact, hip_artifact, base_artifact],
            loader_dependencies: vec![amdhip_artifact, libc_artifact],
            target_os: "linux".into(),
            target_arch: "x86_64".into(),
            os_id: "ubuntu".into(),
            os_version_id: "26.04".into(),
            rocm_version: "7.1.52801-9999".into(),
            gpu_arch: "gfx1151".into(),
        };
        let evidence = LlamaRuntimeEvidence {
            target_os: "linux".into(),
            target_arch: "x86_64".into(),
            os_id: "ubuntu".into(),
            os_version_id: "26.04".into(),
            rocm_version: Some("7.1.52801-9999".into()),
            gpu_arches: vec!["gfx1151".into()],
            binary_ldd: Some(format!(
                "libggml-base.so => {} (0x1)\nlibc.so.6 => {} (0x2)",
                base.display(),
                libc.display()
            )),
            hip_ldd: Some(format!(
                "libamdhip64.so.7 => {} (0x1)\nlibc.so.6 => {} (0x2)",
                amdhip.display(),
                libc.display()
            )),
        };
        StrictFixture {
            _temp: temp,
            policy,
            evidence,
            binary,
            bundle_dir,
            authority_uid,
        }
    }

    #[cfg(unix)]
    fn verify_fixture(
        fixture: &StrictFixture,
    ) -> std::result::Result<LlamaServerSelection, LlamaRuntimePolicyError> {
        verify_llama_runtime_policy_for_authority(
            &fixture.policy,
            &fixture.evidence,
            fixture.authority_uid,
            false,
        )
    }

    #[cfg(unix)]
    #[test]
    fn strict_rocm_manifest_and_hip_identity_pass() {
        let fixture = strict_rocm_fixture();
        let selection = verify_fixture(&fixture).unwrap();
        assert_eq!(selection.backend, LlamaServerBackend::Rocm);
        assert_eq!(selection.path, fixture.binary);
        selection.revalidate().unwrap();

        let mut command = Command::new(&selection.path);
        command
            .env("LD_PRELOAD", "/tmp/injected.so")
            .env("LD_AUDIT", "/tmp/audit.so")
            .env("LD_LIBRARY_PATH", "/tmp/injected");
        selection.configure_command(&mut command);
        let env: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(env.get(std::ffi::OsStr::new("LD_PRELOAD")), Some(&None));
        assert_eq!(env.get(std::ffi::OsStr::new("LD_AUDIT")), Some(&None));
        assert_eq!(
            env.get(std::ffi::OsStr::new("LD_LIBRARY_PATH"))
                .and_then(|value| *value),
            Some(fixture.bundle_dir.as_os_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_rocm_fails_on_platform_hash_and_linkage_mismatch() {
        let mut fixture = strict_rocm_fixture();
        fixture.evidence.gpu_arches = vec!["gfx1150".into()];
        assert!(matches!(
            verify_fixture(&fixture),
            Err(LlamaRuntimePolicyError::PlatformMismatch {
                field: "GPU architecture set",
                ..
            })
        ));

        fixture.evidence.gpu_arches = vec!["gfx1151".into()];
        fixture.evidence.hip_ldd = Some("libamdhip64.so.7 => not found".into());
        assert!(matches!(
            verify_fixture(&fixture),
            Err(LlamaRuntimePolicyError::HipAttestation(_))
        ));

        fixture.evidence.hip_ldd = Some(String::new());
        std::fs::write(&fixture.binary, b"changed after policy creation").unwrap();
        assert!(matches!(
            verify_fixture(&fixture),
            Err(LlamaRuntimePolicyError::HashMismatch { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn strict_rocm_rejects_unknown_bundle_or_loader_dependency() {
        let mut fixture = strict_rocm_fixture();
        std::fs::write(fixture.bundle_dir.join("libggml-evil.so"), b"unknown").unwrap();
        assert!(matches!(
            verify_fixture(&fixture),
            Err(LlamaRuntimePolicyError::Config(_)) | Err(LlamaRuntimePolicyError::Artifact { .. })
        ));
        std::fs::remove_file(fixture.bundle_dir.join("libggml-evil.so")).unwrap();

        let unexpected = fixture._temp.path().join("deps/libunexpected.so");
        std::fs::write(&unexpected, b"unexpected").unwrap();
        fixture
            .evidence
            .binary_ldd
            .as_mut()
            .unwrap()
            .push_str(&format!(
                "\nlibunexpected.so => {} (0x3)",
                unexpected.display()
            ));
        assert!(matches!(
            verify_fixture(&fixture),
            Err(LlamaRuntimePolicyError::HipAttestation(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn strict_selection_revalidation_detects_inode_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = strict_rocm_fixture();
        let selection = verify_fixture(&fixture).unwrap();
        let old = fixture._temp.path().join("old-llama-server");
        std::fs::rename(&fixture.binary, old).unwrap();
        std::fs::write(&fixture.binary, b"pinned llama server").unwrap();
        std::fs::set_permissions(&fixture.binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            selection.revalidate(),
            Err(LlamaRuntimePolicyError::Artifact { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn strict_artifacts_reject_symlinks_hardlinks_and_writable_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let fixture = strict_rocm_fixture();
        let authority_uid = std::fs::metadata(&fixture.binary).unwrap().uid();
        let artifact = match &fixture.policy {
            LlamaServerRuntimePolicy::Rocm {
                bundle_artifacts, ..
            } => bundle_artifacts[0].clone(),
            _ => unreachable!(),
        };

        std::fs::set_permissions(&artifact.path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            validate_pinned_artifact(&artifact, true, authority_uid, false),
            Err(LlamaRuntimePolicyError::Artifact { .. })
        ));
        std::fs::set_permissions(&artifact.path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hardlink = fixture._temp.path().join("hardlink");
        std::fs::hard_link(&artifact.path, hardlink).unwrap();
        assert!(matches!(
            validate_pinned_artifact(&artifact, true, authority_uid, false),
            Err(LlamaRuntimePolicyError::Artifact { .. })
        ));
        std::fs::remove_file(fixture._temp.path().join("hardlink")).unwrap();
        let target = fixture._temp.path().join("symlink-target");
        std::fs::write(&target, b"pinned llama server").unwrap();
        let link = fixture._temp.path().join("symlink");
        symlink(&target, &link).unwrap();
        let linked = PinnedRuntimeArtifact {
            path: link,
            sha256: artifact.sha256,
        };
        assert!(matches!(
            validate_pinned_artifact(&linked, true, authority_uid, false),
            Err(LlamaRuntimePolicyError::Artifact { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn root_policy_cpu_variant_is_explicit_and_forces_zero_gpu_layers() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("cpu-runtime");
        let deps = temp.path().join("deps");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o755)).unwrap();
        let binary = bundle.join("llama-server");
        let libc = deps.join("libc.so.6");
        let binary_artifact = write_artifact(&binary, b"CPU only", true);
        let libc_artifact = write_artifact(&libc, b"libc", false);
        let authority_uid = std::fs::metadata(&binary).unwrap().uid();
        let policy = LlamaServerRuntimePolicy::CpuRollback {
            binary_path: binary.clone(),
            bundle_artifacts: vec![binary_artifact],
            loader_dependencies: vec![libc_artifact],
            target_os: "linux".into(),
            target_arch: "x86_64".into(),
            os_id: "ubuntu".into(),
            os_version_id: "26.04".into(),
        };
        let evidence = LlamaRuntimeEvidence {
            target_os: "linux".into(),
            target_arch: "x86_64".into(),
            os_id: "ubuntu".into(),
            os_version_id: "26.04".into(),
            binary_ldd: Some(format!("libc.so.6 => {} (0x1)", libc.display())),
            ..LlamaRuntimeEvidence::default()
        };
        let selection =
            verify_llama_runtime_policy_for_authority(&policy, &evidence, authority_uid, false)
                .unwrap();
        assert_eq!(selection.backend, LlamaServerBackend::CpuRollback);

        let config = EngineConfig::default();
        let args = build_llama_args(&config, selection.backend);
        let gpu_index = args.iter().position(|arg| arg == "--n-gpu-layers").unwrap();
        assert_eq!(args[gpu_index + 1], "0");

        let mut value = serde_json::to_value(&policy).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("authorized".into(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<LlamaServerRuntimePolicy>(value).is_err());
    }

    #[test]
    fn hip_and_gfx_parsers_require_exact_structured_fields() {
        assert_eq!(
            parse_hip_version("HIP version: 7.1.52801-9999\n"),
            Some("7.1.52801-9999".into())
        );
        assert_eq!(parse_hip_version("ROCm 7.1.52801-9999"), None);
        assert_eq!(
            parse_gfx_names("Name: gfx1151\nMarketing Name: gfx1150\nName: gfx1151-extra"),
            vec!["gfx1151"]
        );
    }

    #[test]
    fn strict_loader_environment_is_exact_and_rejects_injection() {
        let expected = Path::new("/opt/forgefleet/runtime");
        assert!(
            attest_loader_environment(
                b"HOME=/home/logan\0LD_LIBRARY_PATH=/opt/forgefleet/runtime\0",
                expected,
            )
            .is_ok()
        );
        for bad in [
            b"LD_LIBRARY_PATH=/opt/forgefleet/runtime:/tmp\0".as_slice(),
            b"LD_LIBRARY_PATH=/opt/forgefleet/runtime\0LD_PRELOAD=/tmp/inject.so\0".as_slice(),
            b"HOME=/home/logan\0".as_slice(),
        ] {
            assert!(attest_loader_environment(bad, expected).is_err());
        }
    }

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
        let args = build_llama_args(&config, LlamaServerBackend::Legacy);

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
        let args = build_llama_args(&config, LlamaServerBackend::Legacy);
        assert!(args.contains(&"32".to_string()));
        assert!(!args.contains(&"999".to_string()));
    }
}
