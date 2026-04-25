//! Model discovery — find the best models and inference engines for each fleet node.
//!
//! Sources: Ollama library, HuggingFace, web search, independent model sites.
//! Knows which inference engine works on which hardware.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

/// ModelDiscovery — find new models from all sources.
pub struct ModelDiscoveryTool;

#[async_trait]
impl AgentTool for ModelDiscoveryTool {
    fn name(&self) -> &str {
        "ModelDiscovery"
    }
    fn description(&self) -> &str {
        "Discover new LLM models from multiple sources: Ollama library, HuggingFace trending, web search for new releases, and independent model sites. Also researches which inference engine is best for your hardware."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["search_all","search_ollama","search_huggingface","search_web","engine_recommendation","model_card","trending","new_releases"]},
            "query":{"type":"string","description":"Search query (e.g. 'coding models', 'vision model', 'Falcon 3')"},
            "hardware":{"type":"string","enum":["mac_m3_ultra","mac_m4","amd_ryzen_ai_max","nvidia_dgx","intel_cpu","any"],"description":"Target hardware for engine recommendation"},
            "ram_gb":{"type":"number","description":"Available RAM in GB"},
            "model_name":{"type":"string","description":"Specific model name for model_card action"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        let hardware = input
            .get("hardware")
            .and_then(Value::as_str)
            .unwrap_or("any");
        let ram_gb = input.get("ram_gb").and_then(Value::as_u64).unwrap_or(32);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("ForgeFleet-Agent/0.1")
            .build()
            .unwrap_or_default();

        match action {
            "search_all" => {
                if query.is_empty() {
                    return AgentToolResult::err("'query' required for search_all");
                }
                let mut results = Vec::new();

                // 1. HuggingFace
                let hf_url = format!(
                    "https://huggingface.co/api/models?search={}&sort=trending&direction=-1&limit=5&filter=text-generation",
                    urlenc(query)
                );
                if let Ok(resp) = client.get(&hf_url).send().await {
                    if let Ok(models) = resp.json::<Vec<Value>>().await {
                        results.push("## HuggingFace".into());
                        for m in models.iter().take(5) {
                            let id = m.get("id").and_then(Value::as_str).unwrap_or("?");
                            let downloads = m.get("downloads").and_then(Value::as_u64).unwrap_or(0);
                            results.push(format!("  - {id} ({} downloads)", fmt_num(downloads)));
                        }
                    }
                }

                // 2. Ollama search
                let ollama_search = Command::new("ollama")
                    .args(["search", query])
                    .output()
                    .await;
                if let Ok(out) = ollama_search {
                    if out.status.success() {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        results.push("## Ollama".into());
                        for line in stdout.lines().take(6) {
                            results.push(format!("  {line}"));
                        }
                    }
                }

                // 3. Web search for independent models
                results.push("## Other Sources".into());
                results.push("  Search these sites for models not on Ollama/HuggingFace:".into());
                results.push("  - https://falconllm.tii.ae/falcon-models.html (Falcon 3)".into());
                results.push("  - https://www.together.ai/models (Together AI)".into());
                results.push("  - https://openrouter.ai/models (OpenRouter catalog)".into());
                results.push(
                    "  - https://lmsys.org/blog/2024-01-17-swe-bench/ (SWE-bench rankings)".into(),
                );
                results.push(
                    "  - Use WebSearch tool for: 'best coding LLM 2026', 'new open source models'"
                        .into(),
                );

                if results.is_empty() {
                    AgentToolResult::ok("No results. Try a different query.".to_string())
                } else {
                    AgentToolResult::ok(format!(
                        "Model Search: \"{query}\"\n\n{}",
                        results.join("\n")
                    ))
                }
            }

            "search_ollama" => {
                // Ollama has a search command in newer versions
                let result = Command::new("ollama")
                    .args(["search", if query.is_empty() { "coding" } else { query }])
                    .output()
                    .await;
                match result {
                    Ok(out) if out.status.success() => AgentToolResult::ok(format!(
                        "Ollama Search:\n{}",
                        truncate_output(
                            &String::from_utf8_lossy(&out.stdout),
                            MAX_TOOL_RESULT_CHARS
                        )
                    )),
                    _ => {
                        // Fallback: list popular models
                        AgentToolResult::ok(
                            "Ollama Popular Models:\n\n\
  Coding:\n\
    - qwen2.5-coder:32b (20GB) — best open coding model\n\
    - qwen2.5-coder:14b (9GB) — balanced coding\n\
    - codestral:22b (12GB) — Mistral's coding model\n\
    - deepseek-coder-v2:16b (9GB) — strong code generation\n\
    - starcoder2:15b (9GB) — code completion\n\n\
  General:\n\
    - qwen2.5:72b (45GB) — best open general model\n\
    - llama3.2:3b (2GB) — ultra fast, good for simple tasks\n\
    - gemma2:27b (15GB) — Google's model\n\
    - mixtral:8x22b (79GB) — MoE architecture\n\n\
  Vision:\n\
    - llama3.2-vision:11b (7GB) — vision-language\n\
    - gemma-4:31b (20GB) — multimodal\n\n\
  Reasoning:\n\
    - deepseek-r1:32b (20GB) — chain-of-thought\n\
    - qwen3:32b (20GB) — latest Qwen\n\n\
  Use: ollama pull <model> to download"
                                .to_string(),
                        )
                    }
                }
            }

            "search_huggingface" => {
                let hf_query = if query.is_empty() {
                    "text-generation"
                } else {
                    query
                };
                let url = format!(
                    "https://huggingface.co/api/models?search={}&sort=trending&direction=-1&limit=15&filter=text-generation",
                    urlenc(hf_query)
                );
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<Vec<Value>>().await {
                            Ok(models) => {
                                let mut output =
                                    format!("HuggingFace Trending ('{hf_query}'):\n\n");
                                for (i, m) in models.iter().take(15).enumerate() {
                                    let id = m.get("id").and_then(Value::as_str).unwrap_or("?");
                                    let downloads =
                                        m.get("downloads").and_then(Value::as_u64).unwrap_or(0);
                                    let likes = m.get("likes").and_then(Value::as_u64).unwrap_or(0);
                                    output.push_str(&format!(
                                        "  {}. {id} — {} downloads, {} likes\n",
                                        i + 1,
                                        fmt_num(downloads),
                                        likes
                                    ));
                                }
                                output.push_str("\nFor GGUF versions, search: <model>-GGUF (e.g. 'Qwen/Qwen2.5-Coder-32B-Instruct-GGUF')");
                                AgentToolResult::ok(output)
                            }
                            Err(e) => AgentToolResult::err(format!("Parse error: {e}")),
                        }
                    }
                    _ => AgentToolResult::err("HuggingFace API failed".to_string()),
                }
            }

            "trending" | "new_releases" => {
                let url = "https://huggingface.co/api/models?sort=trending&direction=-1&limit=10&filter=text-generation";
                match client.get(url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<Vec<Value>>().await {
                            Ok(models) => {
                                let mut output = String::from("Trending LLMs Right Now:\n\n");
                                for (i, m) in models.iter().take(10).enumerate() {
                                    let id = m.get("id").and_then(Value::as_str).unwrap_or("?");
                                    let downloads =
                                        m.get("downloads").and_then(Value::as_u64).unwrap_or(0);
                                    output.push_str(&format!(
                                        "  {}. {id} ({} downloads)\n",
                                        i + 1,
                                        fmt_num(downloads)
                                    ));
                                }
                                output.push_str("\nUse ModelDiscovery model_card model_name='<id>' for details.");
                                AgentToolResult::ok(output)
                            }
                            Err(_) => AgentToolResult::err("Failed to parse trending".to_string()),
                        }
                    }
                    _ => AgentToolResult::err("HuggingFace API failed".to_string()),
                }
            }

            "engine_recommendation" => {
                let engines = recommend_engines(hardware, ram_gb);
                AgentToolResult::ok(engines)
            }

            "model_card" => {
                let model_name = input
                    .get("model_name")
                    .and_then(Value::as_str)
                    .or(Some(query))
                    .unwrap_or("");
                if model_name.is_empty() {
                    return AgentToolResult::err("'model_name' required");
                }

                let url = format!("https://huggingface.co/api/models/{model_name}");
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                        Ok(model) => {
                            let id = model.get("id").and_then(Value::as_str).unwrap_or("?");
                            let pipeline = model
                                .get("pipeline_tag")
                                .and_then(Value::as_str)
                                .unwrap_or("?");
                            let downloads =
                                model.get("downloads").and_then(Value::as_u64).unwrap_or(0);
                            let likes = model.get("likes").and_then(Value::as_u64).unwrap_or(0);
                            let tags: Vec<&str> = model
                                .get("tags")
                                .and_then(Value::as_array)
                                .map(|a| a.iter().filter_map(Value::as_str).take(15).collect())
                                .unwrap_or_default();
                            let siblings: Vec<&str> = model
                                .get("siblings")
                                .and_then(Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|s| s.get("rfilename").and_then(Value::as_str))
                                        .filter(|f| {
                                            f.ends_with(".gguf") || f.ends_with(".safetensors")
                                        })
                                        .take(10)
                                        .collect()
                                })
                                .unwrap_or_default();

                            let mut output = format!(
                                "Model Card: {id}\n\n\
                                     Pipeline: {pipeline}\n\
                                     Downloads: {}\n\
                                     Likes: {likes}\n\
                                     Tags: {}\n",
                                fmt_num(downloads),
                                tags.join(", ")
                            );

                            if !siblings.is_empty() {
                                output.push_str(&format!(
                                    "\nAvailable files:\n{}\n",
                                    siblings
                                        .iter()
                                        .map(|f| format!("  - {f}"))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                ));
                            }

                            output.push_str(&format!("\nhttps://huggingface.co/{id}"));
                            AgentToolResult::ok(output)
                        }
                        Err(e) => AgentToolResult::err(format!("Parse error: {e}")),
                    },
                    Ok(resp) => {
                        AgentToolResult::err(format!("Model not found: HTTP {}", resp.status()))
                    }
                    Err(e) => AgentToolResult::err(format!("Request failed: {e}")),
                }
            }

            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// Recommend the best inference engine for given hardware.
fn recommend_engines(hardware: &str, ram_gb: u64) -> String {
    let mut output =
        format!("Inference Engine Recommendations for {hardware} ({ram_gb}GB RAM):\n\n");

    match hardware {
        "mac_m3_ultra" | "mac_m4" => {
            output.push_str("## Apple Silicon\n\n");
            output.push_str("  1. **MLX** (BEST for Apple Silicon)\n");
            output.push_str("     - Native Metal GPU acceleration\n");
            output.push_str("     - Fastest inference on Mac\n");
            output.push_str("     - Install: pip install mlx-lm\n");
            output.push_str("     - Run: mlx_lm.server --model <model>\n\n");
            output.push_str("  2. **llama.cpp** (Metal backend)\n");
            output.push_str("     - Very good on Mac, supports GGUF\n");
            output.push_str("     - Widest model compatibility\n");
            output.push_str("     - Run: llama-server -m model.gguf --n-gpu-layers 999\n\n");
            output.push_str("  3. **Ollama** (uses llama.cpp internally)\n");
            output.push_str("     - Easiest setup, good for beginners\n");
            output.push_str("     - Run: ollama serve && ollama run <model>\n\n");
            output.push_str("  ❌ vLLM — does NOT work on Mac (CUDA only)\n");
            output.push_str("  ❌ TensorRT-LLM — does NOT work on Mac (NVIDIA only)\n");
        }
        "amd_ryzen_ai_max" => {
            output.push_str("## AMD Ryzen AI Max+ 395 (128GB LPDDR5X)\n\n");
            output.push_str("  1. **llama.cpp** (ROCm/Vulkan backend) (BEST)\n");
            output.push_str("     - ROCm support for AMD integrated GPU\n");
            output.push_str("     - 128GB unified memory = run 100B+ models\n");
            output.push_str("     - Build: cmake -DGGML_HIP=ON .. && make\n");
            output.push_str("     - Supports RPC for distributed inference\n\n");
            output.push_str("  2. **Ollama** (uses llama.cpp with ROCm)\n");
            output.push_str("     - Easiest setup on AMD\n");
            output.push_str("     - Automatically detects AMD GPU\n\n");
            output.push_str("  3. **mistral.rs** (Rust native)\n");
            output.push_str("     - No Python dependency\n");
            output.push_str("     - ISQ on-the-fly quantization\n\n");
            output.push_str("  ❌ vLLM — limited AMD support (ROCm 6.0+ only)\n");
            output.push_str("  ❌ TensorRT-LLM — NVIDIA only\n");
            output.push_str("  ❌ MLX — Apple Silicon only\n");
        }
        "nvidia_dgx" => {
            output.push_str("## NVIDIA DGX Spark (128GB unified)\n\n");
            output.push_str("  1. **vLLM** (BEST for NVIDIA)\n");
            output.push_str("     - PagedAttention, continuous batching\n");
            output.push_str("     - Tensor parallelism across GPUs\n");
            output.push_str("     - Highest throughput for serving\n");
            output.push_str("     - Run: vllm serve <model> --tensor-parallel-size N\n\n");
            output.push_str("  2. **TensorRT-LLM** (maximum speed)\n");
            output.push_str("     - Compile models to TRT engines\n");
            output.push_str("     - INT4/INT8/FP8 quantization\n");
            output.push_str("     - Fastest possible inference on NVIDIA\n\n");
            output.push_str("  3. **llama.cpp** (CUDA backend)\n");
            output.push_str("     - Good for development/testing\n");
            output.push_str("     - Supports RPC for multi-node\n\n");
            output.push_str("  ❌ MLX — Apple only\n");
        }
        "intel_cpu" => {
            output.push_str("## Intel CPU (no GPU)\n\n");
            output.push_str("  1. **llama.cpp** (CPU backend) (BEST)\n");
            output.push_str("     - Optimized AVX2/AVX512 inference\n");
            output.push_str("     - Q4_K_M quantization for speed\n");
            output.push_str("     - Run: llama-server -m model.gguf -t $(nproc)\n\n");
            output.push_str("  2. **Ollama** (CPU mode)\n");
            output.push_str("     - Same engine, easier setup\n\n");
            output.push_str("  3. **mistral.rs** (CPU mode)\n");
            output.push_str("     - Rust native, ISQ quantization\n\n");
            output.push_str("  ❌ vLLM — requires GPU\n");
            output.push_str("  ❌ MLX — Apple only\n");
            output.push_str("  ❌ TensorRT-LLM — NVIDIA only\n\n");
            output.push_str("  ⚠ CPU inference is slow. Recommend smaller models (7B-14B).\n");
        }
        _ => {
            output.push_str("## General Recommendations\n\n");
            output
                .push_str("  | Engine | Mac | Ubuntu (Intel) | Ubuntu (NVIDIA) | Ubuntu (AMD) |\n");
            output
                .push_str("  |--------|-----|----------------|-----------------|-------------|\n");
            output.push_str("  | llama.cpp | ✅ Metal | ✅ CPU | ✅ CUDA | ✅ ROCm |\n");
            output.push_str("  | Ollama | ✅ | ✅ | ✅ | ✅ |\n");
            output.push_str("  | vLLM | ❌ | ❌ | ✅ Best | ⚠ Limited |\n");
            output.push_str("  | MLX | ✅ Best | ❌ | ❌ | ❌ |\n");
            output.push_str("  | mistral.rs | ✅ | ✅ | ✅ | ✅ |\n");
            output.push_str("  | TensorRT | ❌ | ❌ | ✅ Fastest | ❌ |\n");
        }
    }

    // Model size recommendations based on RAM
    output.push_str(&format!("\n## Model Sizes for {ram_gb}GB RAM:\n"));
    if ram_gb >= 128 {
        output.push_str("  - Can run: 405B (Q4) or multiple 70B models simultaneously\n");
        output.push_str("  - Recommended: Llama-3.1-405B, Qwen2.5-72B, 2× Qwen2.5-Coder-32B\n");
    } else if ram_gb >= 64 {
        output.push_str("  - Can run: 72B (Q4) or 2× 32B models\n");
        output.push_str("  - Recommended: Qwen2.5-72B, Llama-3.1-70B\n");
    } else if ram_gb >= 32 {
        output.push_str("  - Can run: 32B (Q4) or 2× 14B models\n");
        output.push_str("  - Recommended: Qwen2.5-Coder-32B, DeepSeek-R1-32B\n");
    } else if ram_gb >= 16 {
        output.push_str("  - Can run: 14B (Q4) or 7B (Q8)\n");
        output.push_str("  - Recommended: Qwen2.5-Coder-14B, Llama-3.2-8B\n");
    } else {
        output.push_str("  - Can run: 3B-7B models\n");
        output.push_str("  - Recommended: Llama-3.2-3B, Qwen2.5-3B\n");
    }

    output
}

fn urlenc(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

fn fmt_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// ClusterInference — manage distributed LLM inference across fleet nodes.
pub struct ClusterInferenceTool;

#[async_trait]
impl AgentTool for ClusterInferenceTool {
    fn name(&self) -> &str {
        "ClusterInference"
    }
    fn description(&self) -> &str {
        "Set up and manage clustered LLM inference — split one large model across multiple fleet nodes for combined memory. Supports llama.cpp RPC, Exo pipeline parallelism, and vLLM tensor parallelism."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["plan","setup","status","benchmark","teardown"]},
            "model":{"type":"string","description":"Model to cluster (e.g. 'Llama-3.1-405B')"},
            "nodes":{"type":"array","items":{"type":"string"},"description":"Node IPs to include in cluster"},
            "method":{"type":"string","enum":["llamacpp_rpc","exo","vllm_tp","auto"],"description":"Clustering method (default: auto based on hardware)"},
            "port":{"type":"number","description":"API port for the cluster endpoint (default: 51000)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let model = input.get("model").and_then(Value::as_str).unwrap_or("");
        let nodes: Vec<&str> = input
            .get("nodes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("auto");

        match action {
            "plan" => {
                if model.is_empty() { return AgentToolResult::err("'model' required for plan"); }
                if nodes.is_empty() { return AgentToolResult::err("'nodes' (IP list) required"); }

                let node_count = nodes.len();
                let model_lower = model.to_ascii_lowercase();
                let estimated_ram = if model_lower.contains("405b") { 250 }
                    else if model_lower.contains("70b") || model_lower.contains("72b") { 45 }
                    else if model_lower.contains("32b") || model_lower.contains("34b") { 20 }
                    else { 15 };
                let ram_per_node = estimated_ram / node_count;

                let recommended_method = if nodes.iter().all(|n| n.starts_with("192.168.5.1")) {
                    // DGX Sparks or EVO-X2 (high bandwidth) → llama.cpp RPC
                    "llamacpp_rpc"
                } else {
                    "exo"
                };

                let actual_method = if method == "auto" { recommended_method } else { method };

                let mut plan = format!(
                    "Cluster Plan: {model}\n\n\
                     Nodes: {} ({node_count} total)\n\
                     Method: {actual_method}\n\
                     Estimated total RAM needed: ~{estimated_ram}GB (Q4_K_M)\n\
                     RAM per node: ~{ram_per_node}GB\n\n",
                    nodes.join(", ")
                );

                match actual_method {
                    "llamacpp_rpc" => {
                        plan.push_str("## llama.cpp RPC Setup\n\n");
                        plan.push_str(&format!("Controller: {} (runs llama-server)\n", nodes[0]));
                        plan.push_str("Workers:\n");
                        for (_i, node) in nodes.iter().skip(1).enumerate() {
                            plan.push_str(&format!("  {}: rpc-server on port 50052\n", node));
                        }
                        plan.push_str(&format!("\nCommands:\n"));
                        for node in nodes.iter().skip(1) {
                            plan.push_str(&format!("  ssh root@{node} 'rpc-server --host 0.0.0.0 --port 50052' &\n"));
                        }
                        let rpc_addrs: Vec<String> = nodes.iter().skip(1).map(|n| format!("{n}:50052")).collect();
                        plan.push_str(&format!("  ssh root@{} 'llama-server -m /models/{}.gguf --rpc {} --host 0.0.0.0 --port 51000'\n", nodes[0], model.replace(' ', "-"), rpc_addrs.join(",")));
                    }
                    "exo" => {
                        plan.push_str("## Exo Pipeline Parallel Setup\n\n");
                        plan.push_str("Install on all nodes: pip install exo\n\n");
                        for (i, node) in nodes.iter().enumerate() {
                            plan.push_str(&format!("  Node {}: ssh root@{node} 'exo --node-id node{i} --peers {}'\n", i, nodes.iter().filter(|n| *n != node).map(|n| format!("{n}:50051")).collect::<Vec<_>>().join(",")));
                        }
                        plan.push_str(&format!("\nAPI endpoint: http://{}:55000\n", nodes[0]));
                    }
                    "vllm_tp" => {
                        plan.push_str("## vLLM Tensor Parallel Setup\n\n");
                        plan.push_str("⚠ vLLM tensor parallelism requires NVIDIA GPUs with NVLink.\n");
                        plan.push_str("For DGX Sparks, use pipeline parallelism instead.\n\n");
                        plan.push_str(&format!("  vllm serve {model} --tensor-parallel-size {node_count} --pipeline-parallel-size 1\n"));
                    }
                    _ => plan.push_str("Unknown method\n"),
                }

                AgentToolResult::ok(plan)
            }

            "status" => {
                let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap_or_default();
                let mut status = Vec::new();
                for node in &nodes {
                    // Check RPC server
                    let _rpc_url = format!("http://{node}:50052");
                    let rpc_ok = tokio::net::TcpStream::connect(format!("{node}:50052")).await.is_ok();

                    // Check LLM server
                    let llm_url = format!("http://{node}:55000/health");
                    let llm_ok = client.get(&llm_url).send().await.map(|r| r.status().is_success()).unwrap_or(false);

                    status.push(format!("  {node}: RPC={} LLM={}", if rpc_ok {"ON"} else {"OFF"}, if llm_ok {"ON"} else {"OFF"}));
                }
                AgentToolResult::ok(format!("Cluster Status:\n{}", status.join("\n")))
            }

            "benchmark" => {
                AgentToolResult::ok("Cluster Benchmark:\n  Use the Bash tool to run:\n  curl http://<controller>:55000/v1/chat/completions -d '{\"model\":\"...\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}]}'\n  Measure: time to first token, tokens per second, total latency".to_string())
            }

            "teardown" => {
                let mut results = Vec::new();
                for node in &nodes {
                    let kill = Command::new("ssh")
                        .args(["-o", "ConnectTimeout=3", &format!("root@{node}"), "pkill -f 'rpc-server|llama-server|exo' 2>/dev/null; echo done"])
                        .output().await;
                    results.push(format!("  {node}: {}", if kill.map(|o| o.status.success()).unwrap_or(false) { "stopped" } else { "failed" }));
                }
                AgentToolResult::ok(format!("Cluster Teardown:\n{}", results.join("\n")))
            }

            _ => AgentToolResult::err(format!("Unknown action: {action}. Use: plan, setup, status, benchmark, teardown")),
        }
    }
}
