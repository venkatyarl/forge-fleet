//! Misc CLI helpers that don't fit into a specific command domain.

use anyhow::Result;
use std::path::Path;
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
                    let node_models: Vec<_> =
                        models.iter().filter(|m| m.node_name == node.name).collect();
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

/// List fleet nodes from config.
pub fn handle_nodes(p: &Path) -> Result<()> {
    let cfg = crate::utils::load_config(p)?;
    println!("{GREEN}✓ Fleet Nodes{RESET}");
    for (n, d) in cfg.nodes {
        println!("  - {n}: {d}");
    }
    Ok(())
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
