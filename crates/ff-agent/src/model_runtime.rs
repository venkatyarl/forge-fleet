//! Model runtime — start/stop local LLM inference servers and track them in Postgres.
//!
//! Supports three runtimes: llama.cpp (llama-server), MLX (mlx_lm.server), vLLM (vllm serve).
//! Processes are spawned detached from the caller. When loaded, a row is upserted into
//! `fleet_model_deployments` so the rest of the fleet can discover the new endpoint.

use std::path::PathBuf;

/// Options for [`load_model`].
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Library UUID from `fleet_model_library`. Determines which model file to launch.
    pub library_id: String,
    /// Port to bind the inference server on.
    pub port: u16,
    /// Context window size in tokens. Default 32768.
    pub context_size: Option<u32>,
    /// Concurrent parallel request slots (llama.cpp `--parallel`). Default 4.
    pub parallel: Option<u32>,
}

/// Result of a successful load.
#[derive(Debug, Clone)]
pub struct LoadResult {
    pub deployment_id: String,
    pub pid: u32,
    pub runtime: String,
    pub port: u16,
    pub model_path: String,
}

/// A running inference process detected on this host.
#[derive(Debug, Clone)]
pub struct RunningProcess {
    pub pid: u32,
    pub port: Option<u16>,
    pub model_path: Option<String>,
    pub runtime: String,
}

/// Spawn an inference server for the given library row, wait for health, and record
/// the deployment row in Postgres.
pub async fn load_model(pool: &sqlx::PgPool, opts: LoadOptions) -> Result<LoadResult, String> {
    // Find the library row.
    let libs = ff_db::pg_list_library(pool, None)
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;
    let lib = libs
        .into_iter()
        .find(|r| r.id == opts.library_id)
        .ok_or_else(|| format!("no library entry with id '{}'", opts.library_id))?;

    let node_name = crate::fleet_info::resolve_this_node_name().await;
    if lib.node_name != node_name {
        return Err(format!(
            "library row is on '{}', but we're running on '{}'; cross-node load not implemented",
            lib.node_name, node_name
        ));
    }

    let ctx = opts.context_size.unwrap_or(32_768);
    let parallel = opts.parallel.unwrap_or(4);
    let port = opts.port;

    // Build the launch command per runtime.
    let (program, args, runtime_label) = match lib.runtime.as_str() {
        "llama.cpp" => {
            let bin = llama_server_binary();
            let mut args = vec![
                "--model".into(),
                lib.file_path.clone(),
                "--host".into(),
                "0.0.0.0".into(),
                "--port".into(),
                port.to_string(),
                "--ctx-size".into(),
                ctx.to_string(),
                "--parallel".into(),
                parallel.to_string(),
            ];
            // On macOS Metal builds this enables full-GPU inference.
            if cfg!(target_os = "macos") {
                args.push("--n-gpu-layers".into());
                args.push("999".into());
            }
            (bin, args, "llama.cpp")
        }
        "mlx" => {
            // mlx_lm.server expects the MODEL to be either an HF repo id or a local dir
            // with config/weights. We use the local dir.
            let args = vec![
                "--model".into(),
                lib.file_path.clone(),
                "--host".into(),
                "0.0.0.0".into(),
                "--port".into(),
                port.to_string(),
            ];
            ("mlx_lm.server".to_string(), args, "mlx")
        }
        "vllm" => {
            let args = vec![
                "serve".into(),
                lib.file_path.clone(),
                "--host".into(),
                "0.0.0.0".into(),
                "--port".into(),
                port.to_string(),
                "--max-model-len".into(),
                ctx.to_string(),
            ];
            ("vllm".to_string(), args, "vllm")
        }
        other => return Err(format!("unsupported runtime: {other}")),
    };

    tracing::info!(program, ?args, "spawning inference server");

    // Spawn detached (parent doesn't wait). stdout/stderr to log file if present.
    let log_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
        .join(".forgefleet/logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("model-{}.log", port));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open log {}: {e}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| format!("clone log handle: {e}"))?;

    let child = std::process::Command::new(&program)
        .args(&args)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    let pid = child.id();
    // We let the OS parent the process by dropping the handle.
    std::mem::forget(child);

    // Wait for health endpoint to come up (up to 90s).
    let health_ok = wait_for_health(runtime_label, port, std::time::Duration::from_secs(90)).await;

    // Upsert deployment row.
    let deployment_id = ff_db::pg_upsert_deployment(
        pool,
        &node_name,
        Some(&lib.id),
        Some(&lib.catalog_id),
        runtime_label,
        port as i32,
        Some(pid as i32),
        if health_ok { "healthy" } else { "starting" },
        Some(ctx as i32),
    )
    .await
    .map_err(|e| format!("pg_upsert_deployment: {e}"))?;

    if !health_ok {
        tracing::warn!(
            pid,
            port,
            "inference server did not become healthy within 90s"
        );
    }

    Ok(LoadResult {
        deployment_id,
        pid,
        runtime: runtime_label.to_string(),
        port,
        model_path: lib.file_path,
    })
}

/// Stop a running inference server tracked under `deployment_id`.
/// SIGTERM first (up to 10s), then SIGKILL. Deletes the deployment row on success.
pub async fn unload_model(pool: &sqlx::PgPool, deployment_id: &str) -> Result<(), String> {
    let node_name = crate::fleet_info::resolve_this_node_name().await;
    let deployments = ff_db::pg_list_deployments(pool, Some(&node_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    let dep = deployments
        .into_iter()
        .find(|d| d.id == deployment_id)
        .ok_or_else(|| format!("no deployment '{deployment_id}' on this node"))?;

    let pid_i32 = dep.pid.ok_or_else(|| "deployment has no pid".to_string())?;
    let pid = pid_i32 as u32;

    // SIGTERM
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output();

    // Wait up to 10s for process to exit.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !pid_is_alive(pid) {
            break;
        }
    }
    if pid_is_alive(pid) {
        tracing::warn!(pid, "SIGTERM didn't stop process; escalating to SIGKILL");
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .output();
    }

    ff_db::pg_delete_deployment(pool, deployment_id)
        .await
        .map_err(|e| format!("pg_delete_deployment: {e}"))?;
    Ok(())
}

/// Scan local processes for running inference servers (llama.cpp/MLX/vLLM).
pub async fn list_local_processes() -> Vec<RunningProcess> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let (pid_str, rest) = match line.split_once(char::is_whitespace) {
            Some(p) => p,
            None => continue,
        };
        let pid: u32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let runtime = if rest.contains("llama-server") {
            "llama.cpp"
        } else if rest.contains("mlx_lm.server") || rest.contains("mlx_lm/server") {
            "mlx"
        } else if rest.contains("vllm ") && rest.contains("serve") {
            "vllm"
        } else {
            continue;
        };

        // Parse --port <N>
        let port = parse_flag_value(rest, "--port").and_then(|v| v.parse::<u16>().ok());

        // Parse --model <path>, -m <path> (llama-server short form), or positional
        // after `serve` (vllm).
        let model_path = parse_flag_value(rest, "--model")
            .or_else(|| parse_flag_value(rest, "-m"))
            .or_else(|| {
                if runtime == "vllm" {
                    rest.split_once("serve ")
                        .and_then(|(_, after)| after.split_whitespace().next())
                        .map(String::from)
                } else {
                    None
                }
            });

        found.push(RunningProcess {
            pid,
            port,
            model_path,
            runtime: runtime.to_string(),
        });
    }
    found
}

/// Check the health endpoint for a deployment and update `health_status` in Postgres.
/// Returns Ok(true) if healthy, Ok(false) if reachable but unhealthy, Err if unreachable.
pub async fn health_check_deployment(
    pool: &sqlx::PgPool,
    deployment_id: &str,
) -> Result<bool, String> {
    let node_name = crate::fleet_info::resolve_this_node_name().await;
    let deployments = ff_db::pg_list_deployments(pool, Some(&node_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    let dep = deployments
        .into_iter()
        .find(|d| d.id == deployment_id)
        .ok_or_else(|| format!("no deployment '{deployment_id}' on this node"))?;

    let ok = probe_health(
        &dep.runtime,
        dep.port as u16,
        std::time::Duration::from_secs(3),
    )
    .await;
    let status_new = if ok { "healthy" } else { "unhealthy" };

    // Write status back — use upsert with the existing library/catalog/port to update only status.
    let _ = sqlx::query(
        "UPDATE fleet_model_deployments
            SET health_status = $1, last_health_at = NOW()
          WHERE id = $2::uuid",
    )
    .bind(status_new)
    .bind(&dep.id)
    .execute(pool)
    .await
    .map_err(|e| format!("update deployment: {e}"))?;

    Ok(ok)
}

// ─── helpers ──────────────────────────────────────────────────────────────

fn llama_server_binary() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    for rel in [
        "projects/llama.cpp/build/bin/llama-server",
        ".forgefleet/llama.cpp/build/bin/llama-server",
    ] {
        let candidate = PathBuf::from(&home).join(rel);
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    // Fallback: rely on PATH.
    "llama-server".to_string()
}

async fn wait_for_health(runtime: &str, port: u16, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if probe_health(runtime, port, std::time::Duration::from_secs(2)).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
    false
}

/// Public re-export of [`probe_health`] for other modules (e.g. reconciler).
pub async fn probe_health_public(runtime: &str, port: u16, timeout: std::time::Duration) -> bool {
    probe_health(runtime, port, timeout).await
}

async fn probe_health(runtime: &str, port: u16, timeout: std::time::Duration) -> bool {
    // llama.cpp and vllm expose /health; MLX uses /v1/models.
    let endpoint = match runtime {
        "mlx" => "/v1/models",
        _ => "/health",
    };
    let url = format!("http://127.0.0.1:{port}{endpoint}");
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    client
        .get(&url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn parse_flag_value(cmdline: &str, flag: &str) -> Option<String> {
    let mut parts = cmdline.split_whitespace();
    while let Some(p) = parts.next() {
        if p == flag {
            return parts.next().map(String::from);
        }
        if let Some(rest) = p.strip_prefix(&format!("{flag}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn pid_is_alive(pid: u32) -> bool {
    // `kill -0 <pid>` returns 0 if the process exists and we can signal it.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
