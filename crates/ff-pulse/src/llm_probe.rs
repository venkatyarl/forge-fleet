//! LlmProbe — scans localhost ports for running LLM inference servers
//! (llama-server, mlx_lm.server, vllm, ollama) and enumerates model files
//! present in `~/models` but not currently loaded.
//!
//! The probe is intentionally best-effort: any port that does not respond
//! within 1 second is silently skipped. This mirrors the style of
//! `HeartbeatPublisher::scan_local_models` in heartbeat.rs but produces the
//! richer `LlmServer` struct expected by PulseBeatV2.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use tracing::debug;
use uuid::Uuid;

use crate::beat_v2::{AvailableModel, ClusterInfo, LlmMemoryUsage, LlmServer, LlmServerModel};

/// Ports on which ForgeFleet conventionally runs LLM inference servers,
/// plus the well-known Ollama port.
const LLM_SCAN_PORTS: &[u16] = &[
    51001, 51003, 51004, 51005, 51006, 51007, 51008, 51009, 51010, 55000, 55001, 55002, 55003,
    55004, 55005, 55006, 55007, 55008, 55009, 55010, 11434, // ollama
];

/// Ports that belong to ForgeFleet infrastructure, NOT LLM servers.
/// Never classify these as inference endpoints even if they respond to probes.
const EXCLUDED_PORTS: &[u16] = &[
    50000, // openclaw gateway
    50001, // MCP HTTP
    51002, // forgefleetd gateway (responds to /health + empty /v1/models)
    51100, // pulse P2P TCP (future)
];

pub struct LlmProbe;

impl LlmProbe {
    /// Scan localhost ports for running LLM servers, returning fully-populated
    /// `LlmServer` entries for each reachable endpoint.
    pub async fn detect() -> Vec<LlmServer> {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                debug!("llm_probe: failed to build reqwest client: {e}");
                return Vec::new();
            }
        };

        let mut servers = Vec::new();

        for &port in LLM_SCAN_PORTS {
            // Hard skip for ForgeFleet infrastructure ports.
            if EXCLUDED_PORTS.contains(&port) {
                continue;
            }

            let is_ollama = port == 11434;

            // Fetch /v1/models. The definitive signal that a port is an LLM
            // server is a non-empty "data" array. ForgeFleet's own gateway
            // returns {"data":[]} which we must NOT classify as an LLM.
            let models_url = format!("http://127.0.0.1:{port}/v1/models");
            let v1_response = client.get(&models_url).send().await.ok();

            let (model_id, raw_body, has_data, server_header) = match v1_response {
                Some(r) if r.status().is_success() => {
                    let server_header = r
                        .headers()
                        .get("server")
                        .and_then(|h| h.to_str().ok())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let body_txt = r.text().await.unwrap_or_default();
                    let parsed: Option<serde_json::Value> = serde_json::from_str(&body_txt).ok();
                    let data_arr = parsed
                        .as_ref()
                        .and_then(|v| v.get("data"))
                        .and_then(|d| d.as_array());
                    let has_data = data_arr.map(|a| !a.is_empty()).unwrap_or(false);
                    let id = data_arr
                        .and_then(|arr| arr.last())
                        .and_then(|m| m.get("id"))
                        .and_then(|id| id.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    (id, body_txt, has_data, server_header)
                }
                _ => ("unknown".to_string(), String::new(), false, String::new()),
            };

            // Ollama fallback: query /api/tags which lists pulled models.
            // Ollama's /v1/models is OpenAI-compatible but only shows models that have been
            // recently used — /api/tags is the authoritative list.
            let (model_id, raw_body, has_data) = if is_ollama && !has_data {
                let tags_url = format!("http://127.0.0.1:{port}/api/tags");
                match client.get(&tags_url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let body_txt = r.text().await.unwrap_or_default();
                        let parsed: Option<serde_json::Value> =
                            serde_json::from_str(&body_txt).ok();
                        let models_arr = parsed
                            .as_ref()
                            .and_then(|v| v.get("models"))
                            .and_then(|m| m.as_array());
                        let has_models = models_arr.map(|a| !a.is_empty()).unwrap_or(false);
                        let id = models_arr
                            .and_then(|arr| arr.first())
                            .and_then(|m| m.get("name"))
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        (id, body_txt, has_models)
                    }
                    _ => (model_id, raw_body, has_data),
                }
            } else {
                (model_id, raw_body, has_data)
            };

            // Strict: no models advertised ⇒ not an LLM server, skip.
            if !has_data {
                continue;
            }

            let (tokens_per_sec, queue_depth, metrics_runtime_hint) =
                fetch_metrics(&client, port).await;
            let runtime = identify_runtime(
                port,
                &raw_body,
                &server_header,
                &model_id,
                metrics_runtime_hint,
            );

            let display_name = model_id.rsplit('/').next().unwrap_or(&model_id).to_string();

            // If we got this far, the server advertised at least one model → active.
            let status = "active";
            let health_ok = true;

            servers.push(LlmServer {
                deployment_id: Uuid::new_v4(),
                runtime: runtime.to_string(),
                endpoint: format!("http://127.0.0.1:{port}"),
                openai_compatible: !is_ollama || raw_body.contains("\"data\""),
                model: LlmServerModel {
                    id: model_id.clone(),
                    display_name,
                    loaded_path: extract_loaded_path(&raw_body).unwrap_or_default(),
                    context_window: 0,
                    parallel_slots: 0,
                },
                status: status.to_string(),
                pid: None,
                started_at: Utc::now(),
                cluster: ClusterInfo {
                    cluster_id: None,
                    role: "solo".to_string(),
                    tensor_parallel_size: 1,
                    pipeline_parallel_size: 1,
                    peers: Vec::new(),
                },
                queue_depth,
                active_requests: 0,
                tokens_per_sec_last_min: tokens_per_sec,
                gpu_memory_used_gb: None,
                is_healthy: health_ok,
                last_probed_at: Utc::now(),
                memory_used: LlmMemoryUsage {
                    model_weights_gb: 0.0,
                    kv_cache_gb: 0.0,
                    overhead_gb: 0.0,
                    total_gb: 0.0,
                },
            });

            debug!(port, runtime = %runtime, model = %model_id, "detected LLM server");
        }

        servers
    }

    /// Enumerate models present in `~/models/*` (one level deep). Each
    /// subdirectory is treated as one model; file size is summed recursively.
    pub fn available_models() -> Vec<AvailableModel> {
        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        let root = Path::new(&home).join("models");
        let read_dir = match std::fs::read_dir(&root) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                // Support top-level single-file GGUFs as well.
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext == "gguf" {
                        let size_gb = file_size_bytes(&path) as f64 / 1_073_741_824.0;
                        let id = model_id_from_name(
                            &path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("unknown")
                                .to_string(),
                        );
                        out.push(AvailableModel {
                            id,
                            size_gb,
                            runtime_compat: vec!["llama.cpp".into(), "ollama".into()],
                        });
                    }
                }
                continue;
            }

            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let (size_bytes, has_gguf, has_safetensors, has_config_json) = dir_stats(&path);
            // Skip directories that contain no model file indicators.
            if !has_gguf && !has_safetensors && !has_config_json {
                continue;
            }

            let size_gb = size_bytes as f64 / 1_073_741_824.0;
            let mut runtime_compat: Vec<String> = Vec::new();
            if has_gguf {
                runtime_compat.push("llama.cpp".into());
                runtime_compat.push("ollama".into());
            }
            if has_safetensors {
                // mlx_lm only makes sense on Apple Silicon, but we report
                // compatibility; the consumer decides what to launch.
                runtime_compat.push("mlx_lm".into());
                runtime_compat.push("vllm".into());
            }
            if runtime_compat.is_empty() {
                runtime_compat.push("unknown".into());
            }

            out.push(AvailableModel {
                id: model_id_from_name(&dir_name),
                size_gb,
                runtime_compat,
            });
        }

        out
    }
}

// ─── helpers ────────────────────────────────────────────────────────────────

fn identify_runtime(
    port: u16,
    body: &str,
    server_header: &str,
    model_id: &str,
    metrics_hint: Option<&'static str>,
) -> &'static str {
    if port == 11434 {
        return "ollama";
    }
    // /metrics-based detection is the most reliable signal we have:
    // vllm exposes `vllm:*` metric names, llama.cpp exposes `llamacpp:*`.
    // Use whatever the Prometheus scrape told us before falling back to
    // header/body heuristics.
    if let Some(hint) = metrics_hint {
        return hint;
    }
    // llama.cpp advertises itself in the Server header and typically includes
    // .gguf paths in the /v1/models response.
    let server_lc = server_header.to_ascii_lowercase();
    if server_lc.contains("llama.cpp") || body.contains(".gguf") {
        return "llama.cpp";
    }
    // MLX detection: `mlx_lm.server` is a Python BaseHTTPServer and typically
    // serves model ids under `mlx-community/...` or local paths ending in
    // `-mlx` / `-4bit`. Any one of these signatures is sufficient.
    let is_mac = std::env::consts::OS == "macos";
    let header_is_python_basehttp = server_lc.contains("basehttp") && server_lc.contains("python");
    let model_looks_mlx = model_id.contains("mlx-community/")
        || model_id.contains("-mlx")
        || model_id.contains("-4bit");
    let body_looks_mlx =
        body.contains("mlx-community/") || body.contains("-mlx\"") || body.contains("-4bit\"");
    if header_is_python_basehttp || model_looks_mlx || body_looks_mlx {
        return "mlx_lm";
    }
    if body.contains(".safetensors") && is_mac {
        return "mlx_lm";
    }
    // Port 51001 on Linux conventionally runs vllm; on macOS it's mlx_lm.
    if port == 51001 {
        return if is_mac { "mlx_lm" } else { "vllm" };
    }
    // Last-resort fallback: if a Mac host responded with a valid /v1/models
    // payload but didn't match any llama.cpp / ollama signature, it's almost
    // certainly mlx_lm.server. On Linux we still return "unknown" since vllm
    // and others cannot be disambiguated without more signals.
    if is_mac {
        return "mlx_lm";
    }
    "unknown"
}

fn extract_loaded_path(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v.get("data")?.as_array()?;
    let last = arr.last()?;
    let id = last.get("id")?.as_str()?;
    if id.contains('/') {
        Some(id.to_string())
    } else {
        None
    }
}

async fn fetch_metrics(client: &reqwest::Client, port: u16) -> (f64, i32, Option<&'static str>) {
    let url = format!("http://127.0.0.1:{port}/metrics");
    let body = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => return (0.0, 0, None),
    };

    // Prometheus-style scrape. Look for common llama.cpp / vllm metric names.
    // The metric-name prefix is also the most reliable way to identify the
    // runtime — it beats header sniffing (vllm's uvicorn header is generic)
    // and body sniffing (the /v1/models payload looks the same on vllm,
    // mlx_lm, and other OpenAI-shim servers).
    let mut tokens_per_sec = 0.0;
    let mut queue_depth = 0i32;
    let mut runtime_hint: Option<&'static str> = None;

    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        // Split "metric_name value" (ignoring labels for simplicity).
        let (name_part, value_part) = match line.rsplit_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let name = name_part.split('{').next().unwrap_or(name_part);

        // Runtime fingerprint via metric-name prefix. First hit wins.
        if runtime_hint.is_none() {
            if name.starts_with("vllm:") || name.starts_with("vllm_") {
                runtime_hint = Some("vllm");
            } else if name.starts_with("llamacpp:") {
                runtime_hint = Some("llama.cpp");
            }
        }

        let value: f64 = match value_part.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        match name {
            "llamacpp:prompt_tokens_per_second"
            | "vllm:avg_generation_throughput_tokens_per_s"
            | "tokens_per_second" => {
                tokens_per_sec = value;
            }
            "llamacpp:requests_deferred" | "vllm:num_requests_waiting" | "queue_depth" => {
                queue_depth = value as i32;
            }
            _ => {}
        }
    }

    (tokens_per_sec, queue_depth, runtime_hint)
}

fn dir_stats(path: &Path) -> (u64, bool, bool, bool) {
    let mut total: u64 = 0;
    let mut has_gguf = false;
    let mut has_safetensors = false;
    let mut has_config_json = false;

    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let p = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(p),
                Ok(_) => {
                    if let Ok(md) = entry.metadata() {
                        total = total.saturating_add(md.len());
                    }
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if name.ends_with(".gguf") {
                            has_gguf = true;
                        } else if name.ends_with(".safetensors") {
                            has_safetensors = true;
                        } else if name == "config.json" {
                            has_config_json = true;
                        }
                    }
                }
                Err(_) => {}
            }
        }
    }

    (total, has_gguf, has_safetensors, has_config_json)
}

fn file_size_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn model_id_from_name(name: &str) -> String {
    name.to_lowercase().replace(' ', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_identification_fallbacks() {
        assert_eq!(identify_runtime(11434, "", "", "", None), "ollama");
        assert_eq!(
            identify_runtime(55000, "foo.gguf", "", "", None),
            "llama.cpp"
        );
        // On Linux, a bare JSON body with no distinguishing signals falls
        // through to "unknown"; on macOS the last-resort branch returns
        // "mlx_lm". We assert the OS-appropriate expectation.
        let expected_bare = if std::env::consts::OS == "macos" {
            "mlx_lm"
        } else {
            "unknown"
        };
        assert_eq!(identify_runtime(55000, "{}", "", "", None), expected_bare);
        // MLX detection via server header:
        assert_eq!(
            identify_runtime(55000, "{}", "BaseHTTP/0.6 Python/3.14.3", "", None),
            "mlx_lm"
        );
        // MLX detection via model id:
        assert_eq!(
            identify_runtime(
                55000,
                "{}",
                "",
                "mlx-community/Qwen2.5-Coder-32B-4bit",
                None
            ),
            "mlx_lm"
        );
        // llama.cpp server header wins over Mac fallback:
        assert_eq!(
            identify_runtime(55000, "{}", "llama.cpp", "", None),
            "llama.cpp"
        );
        // /metrics hint wins over everything: vllm on port 55000 (Linux) was
        // previously classified "unknown" because there's no body/header
        // signal. Now the Prometheus scrape tells us.
        assert_eq!(
            identify_runtime(55000, "{}", "uvicorn", "", Some("vllm")),
            "vllm"
        );
        // /metrics hint = llama.cpp beats the Mac fallback too:
        assert_eq!(
            identify_runtime(55000, "{}", "", "", Some("llama.cpp")),
            "llama.cpp"
        );
    }

    #[test]
    fn model_id_lowercases_and_dashes() {
        assert_eq!(model_id_from_name("Qwen 2.5 Coder"), "qwen-2.5-coder");
    }

    #[tokio::test]
    async fn detect_returns_vec_when_nothing_listening() {
        // We can't guarantee *no* LLM is running on the test host, so we
        // simply verify the call completes and returns a Vec.
        let _servers = LlmProbe::detect().await;
    }

    #[test]
    fn available_models_handles_missing_dir() {
        // Point HOME at a tempdir with no `models` subdir.
        let tmp = std::env::temp_dir().join(format!("ff-pulse-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("HOME").ok();
        // SAFETY: tests in this crate are single-threaded per process by
        // default; setting HOME here is acceptable for the duration of the
        // test.
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let out = LlmProbe::available_models();
        assert!(out.is_empty());
        unsafe {
            if let Some(h) = prev {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
