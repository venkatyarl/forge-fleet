//! Model management tools — browse, download, compare, and manage LLMs from HuggingFace, Ollama, and fleet endpoints.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult};

/// ModelBrowser — search and browse models from HuggingFace and Ollama.
pub struct ModelBrowserTool;

#[async_trait]
impl AgentTool for ModelBrowserTool {
    fn name(&self) -> &str {
        "ModelBrowser"
    }
    fn description(&self) -> &str {
        "Search and browse LLM models from HuggingFace, Ollama library, or fleet endpoints. View model cards, sizes, capabilities, and compatibility with fleet hardware."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["search","info","list_fleet","list_ollama","list_huggingface","recommend"]},
            "query":{"type":"string","description":"Search query (model name, type, or capability)"},
            "source":{"type":"string","enum":["huggingface","ollama","fleet","all"],"description":"Where to search (default: all)"},
            "task":{"type":"string","description":"Task type for recommendations (coding, reasoning, chat, vision)"},
            "max_ram_gb":{"type":"number","description":"Max RAM available (filters models by size)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");

        match action {
            "search" | "list_huggingface" => {
                // Search HuggingFace API
                let hf_query = if query.is_empty() { "coding" } else { query };
                let url = format!(
                    "https://huggingface.co/api/models?search={}&sort=downloads&direction=-1&limit=10&filter=text-generation",
                    urlencoding(hf_query)
                );
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .unwrap_or_default();
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<Vec<Value>>().await {
                            Ok(models) => {
                                let mut output = format!("HuggingFace Models ('{hf_query}'):\n\n");
                                for (i, model) in models.iter().take(10).enumerate() {
                                    let id = model.get("id").and_then(Value::as_str).unwrap_or("?");
                                    let downloads =
                                        model.get("downloads").and_then(Value::as_u64).unwrap_or(0);
                                    let likes =
                                        model.get("likes").and_then(Value::as_u64).unwrap_or(0);
                                    let pipeline = model
                                        .get("pipeline_tag")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    output.push_str(&format!(
                                        "  {}. {} ({}) — {} downloads, {} likes\n",
                                        i + 1,
                                        id,
                                        pipeline,
                                        format_number(downloads),
                                        likes
                                    ));
                                }
                                AgentToolResult::ok(output)
                            }
                            Err(e) => AgentToolResult::err(format!("Parse error: {e}")),
                        }
                    }
                    _ => AgentToolResult::err("HuggingFace API request failed".to_string()),
                }
            }

            "list_ollama" => {
                // List models from Ollama library
                match Command::new("ollama").arg("list").output().await {
                    Ok(out) if out.status.success() => AgentToolResult::ok(format!(
                        "Ollama Models (local):\n{}",
                        String::from_utf8_lossy(&out.stdout)
                    )),
                    _ => {
                        // Try the Ollama library API
                        let client = reqwest::Client::builder()
                            .timeout(std::time::Duration::from_secs(10))
                            .build()
                            .unwrap_or_default();
                        match client.get("https://ollama.com/api/tags").send().await {
                            Ok(resp) if resp.status().is_success() => AgentToolResult::ok(format!(
                                "Ollama: Use 'ollama pull <model>' to download. Popular: qwen2.5-coder, llama3.2, gemma2, codestral, deepseek-coder-v2"
                            )),
                            _ => AgentToolResult::err(
                                "Ollama not installed or API unreachable".to_string(),
                            ),
                        }
                    }
                }
            }

            "list_fleet" => {
                // Query all fleet LLM endpoints
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap_or_default();
                let nodes = [
                    ("Taylor", "192.168.5.100:55000"),
                    ("Taylor-2", "192.168.5.100:55001"),
                    ("Marcus", "192.168.5.102:55000"),
                    ("Sophie", "192.168.5.103:55000"),
                    ("Priya", "192.168.5.104:55000"),
                    ("James", "192.168.5.108:55000"),
                    ("James-2", "192.168.5.108:55001"),
                ];
                let mut output = String::from("Fleet Models:\n\n");
                for (name, addr) in &nodes {
                    let url = format!("http://{addr}/v1/models");
                    match client.get(&url).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(data) = resp.json::<Value>().await {
                                let models: Vec<String> = data
                                    .get("data")
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|m| {
                                                m.get("id")
                                                    .and_then(Value::as_str)
                                                    .map(String::from)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                output.push_str(&format!("  {name}: {}\n", models.join(", ")));
                            }
                        }
                        _ => output.push_str(&format!("  {name}: offline\n")),
                    }
                }
                AgentToolResult::ok(output)
            }

            "info" => {
                if query.is_empty() {
                    return AgentToolResult::err("'query' (model name) required for info");
                }
                // Get model info from HuggingFace
                let url = format!("https://huggingface.co/api/models/{query}");
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_default();
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                        Ok(model) => {
                            let id = model.get("id").and_then(Value::as_str).unwrap_or("?");
                            let downloads =
                                model.get("downloads").and_then(Value::as_u64).unwrap_or(0);
                            let likes = model.get("likes").and_then(Value::as_u64).unwrap_or(0);
                            let pipeline = model
                                .get("pipeline_tag")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let tags: Vec<&str> = model
                                .get("tags")
                                .and_then(Value::as_array)
                                .map(|a| a.iter().filter_map(Value::as_str).take(10).collect())
                                .unwrap_or_default();
                            let card = model
                                .get("cardData")
                                .and_then(|c| c.get("license"))
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");

                            AgentToolResult::ok(format!(
                                "Model: {id}\nPipeline: {pipeline}\nLicense: {card}\nDownloads: {}\nLikes: {likes}\nTags: {}\n\nhttps://huggingface.co/{id}",
                                format_number(downloads),
                                tags.join(", ")
                            ))
                        }
                        Err(e) => AgentToolResult::err(format!("Parse error: {e}")),
                    },
                    _ => AgentToolResult::err(format!("Model '{query}' not found on HuggingFace")),
                }
            }

            "recommend" => {
                let task = input
                    .get("task")
                    .and_then(Value::as_str)
                    .unwrap_or("coding");
                let max_ram = input
                    .get("max_ram_gb")
                    .and_then(Value::as_u64)
                    .unwrap_or(32);

                let recommendations = match task {
                    "coding" => vec![
                        (
                            "Qwen2.5-Coder-32B (Q4_K_M)",
                            20,
                            "Best coding model for 32GB+ nodes",
                        ),
                        ("Qwen3-Coder-Next", 20, "Latest Qwen coding model"),
                        ("DeepSeek-Coder-V2-16B", 10, "Strong coding, fits 16GB"),
                        ("CodeLlama-34B (Q4_K_M)", 20, "Meta's coding model"),
                    ],
                    "reasoning" => vec![
                        ("Qwen2.5-72B (Q4_K_M)", 45, "Best open reasoning model"),
                        ("Llama-3.1-70B (Q4_K_M)", 45, "Meta's flagship"),
                        ("DeepSeek-R1-32B", 20, "Strong reasoning, smaller"),
                    ],
                    "vision" => vec![
                        ("Gemma-4-31B", 20, "Google multimodal model"),
                        (
                            "Llama-3.2-Vision-11B",
                            8,
                            "Vision-language, fits small nodes",
                        ),
                    ],
                    _ => vec![
                        ("Qwen2.5-Coder-32B (Q4_K_M)", 20, "Best general coding"),
                        ("Gemma-4-31B", 20, "Strong all-around"),
                    ],
                };

                let mut output =
                    format!("Model Recommendations for '{task}' (max {max_ram}GB RAM):\n\n");
                for (name, ram_needed, desc) in &recommendations {
                    let fits = *ram_needed <= max_ram as usize;
                    let icon = if fits { "✓" } else { "✗" };
                    output.push_str(&format!("  {icon} {name} ({ram_needed}GB) — {desc}\n"));
                }
                AgentToolResult::ok(output)
            }

            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// ModelDownloader — download GGUF models from HuggingFace or Ollama.
pub struct ModelDownloaderTool;

#[async_trait]
impl AgentTool for ModelDownloaderTool {
    fn name(&self) -> &str {
        "ModelDownloader"
    }
    fn description(&self) -> &str {
        "Download LLM models to local storage or fleet nodes. Supports Ollama pull, HuggingFace download, and direct GGUF URLs."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "model":{"type":"string","description":"Model name (e.g. 'qwen2.5-coder:32b' for Ollama, 'Qwen/Qwen2.5-Coder-32B-Instruct-GGUF' for HuggingFace)"},
            "method":{"type":"string","enum":["ollama","huggingface","url"],"description":"Download method (default: auto-detect)"},
            "destination":{"type":"string","description":"Download directory (default: ~/models)"},
            "quantization":{"type":"string","description":"GGUF quantization (e.g. Q4_K_M, Q5_K_M, Q8_0)"},
            "node":{"type":"string","description":"Fleet node to download to (IP address, downloads via SSH)"}
        },"required":["model"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let model = input.get("model").and_then(Value::as_str).unwrap_or("");
        let dest = input
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or("~/models");
        let node = input.get("node").and_then(Value::as_str);
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("auto");

        if model.is_empty() {
            return AgentToolResult::err("'model' required");
        }

        let detected_method = if method != "auto" {
            method.to_string()
        } else if model.contains(':') || !model.contains('/') {
            "ollama".into()
        } else if model.starts_with("http") {
            "url".into()
        } else {
            "huggingface".into()
        };

        let cmd = match detected_method.as_str() {
            "ollama" => format!("ollama pull {model}"),
            "huggingface" => {
                let quant = input
                    .get("quantization")
                    .and_then(Value::as_str)
                    .unwrap_or("Q4_K_M");
                format!("huggingface-cli download {model} --include '*{quant}*' --local-dir {dest}")
            }
            "url" => format!("mkdir -p {dest} && cd {dest} && wget -c '{model}'"),
            _ => return AgentToolResult::err(format!("Unknown method: {detected_method}")),
        };

        // Remote or local execution
        let full_cmd = if let Some(node_ip) = node {
            format!("ssh -o StrictHostKeyChecking=no root@{node_ip} '{cmd}'")
        } else {
            cmd.clone()
        };

        AgentToolResult::ok(format!(
            "Model download command:\n  {full_cmd}\n\n\
             Run this via the Bash tool to start the download.\n\
             For large models (>20GB), this may take 10-30 minutes depending on connection speed.\n\
             Estimated sizes: Q4_K_M ≈ 60% of full, Q5_K_M ≈ 70%, Q8_0 ≈ 100%"
        ))
    }
}

/// ModelCompare — compare models side by side.
pub struct ModelCompareTool;

#[async_trait]
impl AgentTool for ModelCompareTool {
    fn name(&self) -> &str {
        "ModelCompare"
    }
    fn description(&self) -> &str {
        "Compare LLM models: parameter count, context window, speed, quality benchmarks, RAM requirements, and fleet compatibility."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "models":{"type":"array","items":{"type":"string"},"description":"Model names to compare"},
            "include_benchmarks":{"type":"boolean","description":"Include benchmark scores (default: true)"}
        },"required":["models"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let models: Vec<&str> = input
            .get("models")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        if models.len() < 2 {
            return AgentToolResult::err("Need at least 2 models to compare");
        }

        let mut output = String::from("Model Comparison:\n\n");
        output.push_str("| Model | Params | Context | RAM (Q4) | Best For |\n");
        output.push_str("|-------|--------|---------|----------|----------|\n");

        for model in &models {
            let (params, context, ram, best_for) = estimate_model_specs(model);
            output.push_str(&format!(
                "| {model} | {params} | {context} | {ram} | {best_for} |\n"
            ));
        }

        output.push_str(
            "\nNote: RAM estimates are for Q4_K_M quantization. Full precision requires ~2x more.",
        );
        AgentToolResult::ok(output)
    }
}

fn estimate_model_specs(model: &str) -> (&str, &str, &str, &str) {
    let lower = model.to_ascii_lowercase();
    if lower.contains("72b") || lower.contains("70b") {
        ("72B", "32K", "~45GB", "Complex reasoning")
    } else if lower.contains("32b") || lower.contains("34b") {
        ("32B", "32K", "~20GB", "Coding & analysis")
    } else if lower.contains("27b") || lower.contains("31b") {
        ("~30B", "32K-262K", "~18GB", "General purpose")
    } else if lower.contains("14b") || lower.contains("16b") {
        ("14-16B", "32K", "~10GB", "Balanced speed/quality")
    } else if lower.contains("9b") || lower.contains("8b") || lower.contains("7b") {
        ("7-9B", "32K", "~5GB", "Fast, lightweight")
    } else if lower.contains("3b") {
        ("3B", "32K", "~2GB", "Ultra-fast, edge")
    } else if lower.contains("405b") {
        ("405B", "128K", "~250GB", "Maximum quality")
    } else {
        ("?", "?", "?", "Unknown")
    }
}

fn format_number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn urlencoding(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}
