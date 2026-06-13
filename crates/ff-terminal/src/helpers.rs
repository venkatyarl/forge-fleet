//! Misc CLI helpers that don't fit into a specific command domain.

use anyhow::Result;
use std::time::Duration;

use crate::{GREEN, RESET};

/// Detect if input is a dropped file/folder path and wrap with appropriate context.
pub fn detect_dropped_content(input: &str) -> String {
    let trimmed = input.trim().trim_matches('\'').trim_matches('"');
    let path = std::path::Path::new(trimmed);

    // Only trigger if it looks like an absolute path that exists
    if !trimmed.starts_with('/') || !path.exists() {
        return input.to_string();
    }

    if path.is_dir() {
        format!(
            "I've dropped a folder: {trimmed}\nPlease explore this directory and tell me what's in it. Use Glob and Read to understand the contents."
        )
    } else {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            // Images
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => {
                format!(
                    "I've dropped an image: {trimmed}\nPlease analyze this image using PhotoAnalysis with file_path=\"{trimmed}\""
                )
            }
            // Videos
            "mp4" | "mov" | "avi" | "mkv" | "webm" => {
                format!(
                    "I've dropped a video: {trimmed}\nPlease analyze this video using VideoAnalysis with file_path=\"{trimmed}\" action=\"info\""
                )
            }
            // Audio
            "mp3" | "wav" | "flac" | "m4a" | "ogg" => {
                format!(
                    "I've dropped an audio file: {trimmed}\nPlease analyze using AudioAnalysis with file_path=\"{trimmed}\" action=\"info\""
                )
            }
            // PDFs
            "pdf" => {
                format!(
                    "I've dropped a PDF: {trimmed}\nPlease extract and summarize the content using PdfExtract with file_path=\"{trimmed}\""
                )
            }
            // Spreadsheets
            "csv" | "xlsx" | "xls" => {
                format!(
                    "I've dropped a spreadsheet: {trimmed}\nPlease read and summarize using SpreadsheetQuery with file_path=\"{trimmed}\" action=\"head\""
                )
            }
            // Code/text files — just read them
            _ => {
                format!(
                    "I've dropped a file: {trimmed}\nPlease read and analyze this file using Read with file_path=\"{trimmed}\""
                )
            }
        }
    }
}

/// Pick a healthy AGENT-CAPABLE endpoint (tool-calling + `usable_agent_ctx >=
/// min_ctx`) from `fleet_model_deployments`, so `ff run` agent-mode routes to
/// an endpoint whose per-slot context actually fits the tool-schema system
/// prompt — instead of the inference router's local-first pick, which can be a
/// small per-slot-ctx endpoint that overflows on turn 1 (P0.1, surfaced
/// 2026-06-08). Returns `None` on any error / no DB / no match, so the caller
/// falls back to its existing routing. Fail-closed — never worse than today.
pub async fn pick_agent_capable_url(config_path: &std::path::Path, min_ctx: i32) -> Option<String> {
    let toml_str = tokio::fs::read_to_string(config_path).await.ok()?;
    let config = toml::from_str::<ff_core::config::FleetConfig>(&toml_str).ok()?;
    let db_url = config.database.url.trim();
    if db_url.is_empty() {
        return None;
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect(db_url)
        .await
        .ok()?;
    ff_db::pg_pick_agent_endpoint(&pool, min_ctx, &[])
        .await
        .ok()
        .flatten()
        .map(|c| c.endpoint)
}

/// Detect the best LLM endpoint by querying Postgres for fleet nodes + models,
/// then probing each for a healthy connection. Falls back to localhost:55000.
pub async fn detect_llm_from_db_or_local(config_path: &std::path::Path) -> String {
    // Try to load fleet.toml to get the database URL
    if let Ok(toml_str) = tokio::fs::read_to_string(config_path).await
        && let Ok(config) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str)
    {
        let db_url = config.database.url.trim();
        if !db_url.is_empty() {
            // Query Postgres for fleet nodes and their model ports
            if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(3))
                .connect(db_url)
                .await
                && let Ok(nodes) = ff_db::pg_list_nodes(&pool).await
            {
                // Also get models to find ports
                let models = ff_db::pg_list_models(&pool).await.unwrap_or_default();

                // Build (ip, port, cores, supports_tools) pairs
                // Prefer models that support tool calling (Qwen) over those that don't (Gemma)
                let mut endpoints: Vec<(String, u16, i32, bool)> = Vec::new();
                for node in &nodes {
                    let node_models: Vec<_> = models
                        .iter()
                        .filter(|m| m.worker_name == node.name)
                        .collect();
                    if node_models.is_empty() {
                        endpoints.push((node.ip.clone(), 55000, node.cpu_cores, true));
                    } else {
                        for m in node_models {
                            // Qwen and Gemma-4 (via MLX) both support OpenAI tool calling.
                            // Check id/slug/name for "gemma-4" or "gemma4" to distinguish from older Gemma variants.
                            let fam = m.family.to_lowercase();
                            let id_lower = m.id.to_lowercase();
                            let name_lower = m.name.to_lowercase();
                            let is_gemma4 = (id_lower.contains("gemma-4")
                                || id_lower.contains("gemma4")
                                || name_lower.contains("gemma-4")
                                || name_lower.contains("gemma4"))
                                && fam.contains("gemma");
                            let supports_tools = fam.contains("qwen") || is_gemma4;
                            endpoints.push((
                                node.ip.clone(),
                                m.port as u16,
                                node.cpu_cores,
                                supports_tools,
                            ));
                        }
                    }
                }
                // Sort: tool-calling models first, then by cores descending
                endpoints.sort_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)));

                for (ip, port, _, _) in &endpoints {
                    if let Ok(addr) = format!("{ip}:{port}").parse()
                        && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200))
                            .is_ok()
                    {
                        tracing::info!(ip = %ip, port, "auto-detected LLM endpoint from database");
                        return format!("http://{ip}:{port}");
                    }
                }
            }
        }
    }

    // Fallback: probe localhost
    for port in [55000, 55001, 11434] {
        if let Ok(addr) = format!("127.0.0.1:{port}").parse()
            && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
        {
            return format!("http://127.0.0.1:{port}");
        }
    }

    "http://localhost:55000".into()
}

/// `ff nodes` — list fleet nodes with hardware/GPU from Postgres.
///
/// Reads the worker registry joined to the `computers` hardware table, so GPU
/// vendor/VRAM and true RAM are visible without SSH-probing. `--gpu <kind>`
/// filters by GPU kind substring (e.g. `--gpu amd` → amd_rocm boxes).
pub async fn handle_nodes(gpu: Option<&str>, json: bool) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    let mut nodes = ff_db::pg_list_nodes(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("pg_list_nodes: {e}"))?;

    // Empty/whitespace --gpu would substring-match every host (contains("")
    // is always true), silently behaving like no filter — guard against it.
    if let Some(g) = gpu.map(str::trim).filter(|g| !g.is_empty()) {
        let g = g.to_lowercase();
        nodes.retain(|n| {
            n.gpu_kind
                .as_deref()
                .map(|k| k.to_lowercase().contains(&g))
                .unwrap_or(false)
        });
    }

    // Sort by primary IP, numerically by octet (fleet-table convention).
    nodes.sort_by_key(|n| ip_sort_key(&n.ip));

    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(());
    }

    if nodes.is_empty() {
        println!(
            "(no nodes{})",
            gpu.map(|g| format!(" matching gpu~{g}"))
                .unwrap_or_default()
        );
        return Ok(());
    }

    println!("{GREEN}✓ Fleet Nodes{RESET}");
    println!(
        "{:<10} {:<15} {:<13} {:>4} {:>6} {:<14} {:>7} {:<8}",
        "NODE", "IP", "OS", "CPU", "RAM", "GPU", "VRAM", "STATUS"
    );
    for n in &nodes {
        let ram = n.computer_ram_gb.unwrap_or(n.ram_gb);
        let cpu = n.computer_cpu_cores.unwrap_or(n.cpu_cores);
        let gpu_kind = n.gpu_kind.as_deref().unwrap_or("-");
        // Prefer total VRAM; per-GPU gpu_vram_gb is NULL for unified-memory
        // boxes (Apple Silicon, GB10) by design.
        let vram = n
            .gpu_total_vram_gb
            .or(n.gpu_vram_gb)
            .filter(|v| *v > 0.0)
            .map(|v| format!("{v:.0}G"))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<10} {:<15} {:<13} {:>4} {:>5}G {:<14} {:>7} {:<8}",
            n.name, n.ip, n.os, cpu, ram, gpu_kind, vram, n.status
        );
    }
    Ok(())
}

/// Sort key for an IPv4 string: the address as a big-endian u32. Anything that
/// isn't a valid IPv4 (IPv6, hostname, out-of-range octet) sorts last. Parsing
/// the whole string atomically avoids silently shifting octets on bad input.
///
/// Shared by every per-computer table (`ff fleet nodes`, `ff fleet health`, …)
/// so they all read in subnet order — the fleet-table convention.
pub(crate) fn ip_sort_key(ip: &str) -> u32 {
    ip.parse::<std::net::Ipv4Addr>()
        .map(u32::from)
        .unwrap_or(u32::MAX)
}

/// Sort raw query rows by their `primary_ip` column in numeric-octet order
/// (the per-computer-table convention), so a `JOIN computers` listing reads in
/// subnet order rather than lexically by name. Callers keep `ORDER BY c.name …`
/// in SQL as a stable secondary key; this stable sort preserves it within an IP.
/// The column must be selected as `primary_ip` (nullable → sorts last).
pub(crate) fn sort_rows_by_primary_ip(rows: &mut [sqlx::postgres::PgRow]) {
    use sqlx::Row;
    rows.sort_by_key(|r| {
        ip_sort_key(
            r.try_get::<Option<String>, _>("primary_ip")
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or(""),
        )
    });
}

/// Detect the OS family of the current host.
pub fn detect_os_family() -> String {
    if cfg!(target_os = "macos") {
        "macos".into()
    } else if cfg!(target_os = "linux") {
        "linux".into()
    } else {
        "unknown".into()
    }
}

#[cfg(test)]
mod tests {
    use super::ip_sort_key;

    #[test]
    fn sorts_numerically_by_octet_not_lexically() {
        // The fleet lives on 192.168.5.x; lexical order would wrongly put
        // ".100" before ".99" and ".116" before ".9". Octet order must not.
        let mut ips = vec![
            "192.168.5.119",
            "192.168.5.9",
            "192.168.5.100",
            "192.168.5.102",
            "192.168.5.99",
        ];
        ips.sort_by_key(|s| ip_sort_key(s));
        assert_eq!(
            ips,
            vec![
                "192.168.5.9",
                "192.168.5.99",
                "192.168.5.100",
                "192.168.5.102",
                "192.168.5.119",
            ]
        );
    }

    #[test]
    fn non_ipv4_sorts_last() {
        // Hostnames / IPv6 / malformed addresses park at the end instead of
        // silently colliding with a real low address.
        assert_eq!(ip_sort_key("not-an-ip"), u32::MAX);
        assert_eq!(ip_sort_key(""), u32::MAX);
        assert_eq!(ip_sort_key("::1"), u32::MAX);
        assert!(ip_sort_key("192.168.5.119") < ip_sort_key("not-an-ip"));
    }
}
