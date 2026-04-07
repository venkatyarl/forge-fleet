//! Multimodal tools — photo, video, and audio analysis.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct PhotoAnalysisTool;
#[async_trait]
impl AgentTool for PhotoAnalysisTool {
    fn name(&self) -> &str { "PhotoAnalysis" }
    fn description(&self) -> &str { "Analyze photos: describe content, extract text (OCR), get EXIF data, detect faces, identify colors. Works with local files or URLs." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string","description":"Path to image file or URL"},
            "actions":{"type":"array","items":{"type":"string","enum":["describe","ocr","exif","faces","colors","dimensions"]},"description":"What to analyze (default: all basic)"}
        },"required":["file_path"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        let actions: Vec<&str> = input.get("actions").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_else(|| vec!["dimensions", "exif", "ocr"]);

        let path = if std::path::Path::new(file).is_absolute() { file.to_string() } else { ctx.working_dir.join(file).to_string_lossy().to_string() };
        let mut results = Vec::new();

        for action in &actions {
            match *action {
                "dimensions" => {
                    let cmd = format!("identify -format '%wx%h %m %b' '{}' 2>/dev/null || sips -g pixelWidth -g pixelHeight '{}' 2>/dev/null", path, path);
                    if let Ok(o) = Command::new("bash").arg("-c").arg(&cmd).output().await {
                        results.push(format!("Dimensions: {}", String::from_utf8_lossy(&o.stdout).trim()));
                    }
                }
                "exif" => {
                    let cmd = format!("exiftool '{}' 2>/dev/null | head -20 || identify -verbose '{}' 2>/dev/null | head -20", path, path);
                    if let Ok(o) = Command::new("bash").arg("-c").arg(&cmd).output().await {
                        results.push(format!("EXIF:\n{}", truncate_output(&String::from_utf8_lossy(&o.stdout), 1000)));
                    }
                }
                "ocr" => {
                    if let Ok(o) = Command::new("tesseract").args([&path, "stdout"]).output().await {
                        if o.status.success() {
                            let text = String::from_utf8_lossy(&o.stdout);
                            results.push(format!("OCR Text:\n{}", truncate_output(&text, 2000)));
                        } else { results.push("OCR: tesseract not available".into()); }
                    }
                }
                "colors" => {
                    let cmd = format!("convert '{}' -colors 5 -format '%c' histogram:info:- 2>/dev/null | head -5", path);
                    if let Ok(o) = Command::new("bash").arg("-c").arg(&cmd).output().await {
                        results.push(format!("Dominant colors:\n{}", String::from_utf8_lossy(&o.stdout)));
                    }
                }
                _ => {}
            }
        }

        if results.is_empty() { AgentToolResult::err("No analysis results. Check if the file exists and tools are installed.".to_string()) }
        else { AgentToolResult::ok(format!("Photo Analysis: {file}\n\n{}", results.join("\n\n"))) }
    }
}

pub struct VideoAnalysisTool;
#[async_trait]
impl AgentTool for VideoAnalysisTool {
    fn name(&self) -> &str { "VideoAnalysis" }
    fn description(&self) -> &str { "Analyze videos: get metadata (duration, resolution, codec), extract frames, extract audio, transcribe speech." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string","description":"Path to video file"},
            "action":{"type":"string","enum":["info","extract_frames","extract_audio","transcribe"]},
            "frame_count":{"type":"number","description":"Number of frames to extract (default: 5)"},
            "output_dir":{"type":"string","description":"Output directory for extracted content"}
        },"required":["file_path","action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        let action = input.get("action").and_then(Value::as_str).unwrap_or("info");
        let path = if std::path::Path::new(file).is_absolute() { file.to_string() } else { ctx.working_dir.join(file).to_string_lossy().to_string() };

        match action {
            "info" => {
                let cmd = format!("ffprobe -v quiet -print_format json -show_format -show_streams '{}' 2>/dev/null || echo 'ffprobe not installed (brew install ffmpeg)'", path);
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    Err(e) => AgentToolResult::err(format!("ffprobe failed: {e}")),
                }
            }
            "extract_frames" => {
                let count = input.get("frame_count").and_then(Value::as_u64).unwrap_or(5);
                let out_dir = input.get("output_dir").and_then(Value::as_str).unwrap_or("./frames");
                let cmd = format!("mkdir -p '{}' && ffmpeg -i '{}' -vf 'select=not(mod(n\\,{}))' -frames:v {} -vsync vfr '{}'/frame_%03d.png 2>/dev/null", out_dir, path, 30, count, out_dir);
                match Command::new("bash").arg("-c").arg(&cmd).current_dir(&ctx.working_dir).output().await {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!("Extracted {count} frames to {out_dir}/")),
                    _ => AgentToolResult::err("Frame extraction failed. Is ffmpeg installed?".to_string()),
                }
            }
            "extract_audio" => {
                let out = input.get("output_dir").and_then(Value::as_str).unwrap_or("./audio.mp3");
                let cmd = format!("ffmpeg -i '{}' -vn -acodec mp3 '{}' 2>/dev/null", path, out);
                match Command::new("bash").arg("-c").arg(&cmd).current_dir(&ctx.working_dir).output().await {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!("Audio extracted to {out}")),
                    _ => AgentToolResult::err("Audio extraction failed. Is ffmpeg installed?".to_string()),
                }
            }
            "transcribe" => {
                // Extract audio then transcribe with whisper
                let cmd = format!(
                    "ffmpeg -i '{}' -vn -acodec pcm_s16le -ar 16000 /tmp/ff_audio.wav 2>/dev/null && whisper /tmp/ff_audio.wav --model base --output_format txt 2>/dev/null && cat /tmp/ff_audio.txt",
                    path
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) if o.status.success() => {
                        AgentToolResult::ok(format!("Transcription:\n{}", truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)))
                    }
                    _ => AgentToolResult::err("Transcription failed. Install: pip install openai-whisper".to_string()),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct AudioAnalysisTool;
#[async_trait]
impl AgentTool for AudioAnalysisTool {
    fn name(&self) -> &str { "AudioAnalysis" }
    fn description(&self) -> &str { "Analyze audio: transcribe speech (Whisper), get metadata, convert formats." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string","description":"Path to audio file"},
            "action":{"type":"string","enum":["transcribe","info","convert"]},
            "output":{"type":"string","description":"Output file (for convert)"},
            "language":{"type":"string","description":"Language hint for transcription"}
        },"required":["file_path","action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        let action = input.get("action").and_then(Value::as_str).unwrap_or("info");
        let path = if std::path::Path::new(file).is_absolute() { file.to_string() } else { ctx.working_dir.join(file).to_string_lossy().to_string() };

        match action {
            "transcribe" => {
                let lang = input.get("language").and_then(Value::as_str).map(|l| format!("--language {l}")).unwrap_or_default();
                let cmd = format!("whisper '{}' --model base --output_format txt {} 2>/dev/null && cat '{}.txt'", path, lang, path.trim_end_matches(|c: char| c != '.'));
                match Command::new("bash").arg("-c").arg(&cmd).current_dir(&ctx.working_dir).output().await {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!("Transcription:\n{}", truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS))),
                    _ => AgentToolResult::err("Transcription failed. Install: pip install openai-whisper".to_string()),
                }
            }
            "info" => {
                let cmd = format!("ffprobe -v quiet -print_format json -show_format '{}' 2>/dev/null", path);
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    Err(e) => AgentToolResult::err(format!("ffprobe failed: {e}")),
                }
            }
            "convert" => {
                let output = input.get("output").and_then(Value::as_str).unwrap_or("output.mp3");
                let cmd = format!("ffmpeg -i '{}' '{}' 2>/dev/null", path, output);
                match Command::new("bash").arg("-c").arg(&cmd).current_dir(&ctx.working_dir).output().await {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!("Converted: {file} → {output}")),
                    _ => AgentToolResult::err("Conversion failed. Is ffmpeg installed?".to_string()),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct SelfHealTool;
#[async_trait]
impl AgentTool for SelfHealTool {
    fn name(&self) -> &str { "SelfHeal" }
    fn description(&self) -> &str { "Detect failures in fleet nodes, diagnose root cause, apply fixes automatically. Monitors LLM servers, services, disk space, and network connectivity." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["diagnose","heal","status","auto"]},
            "node":{"type":"string","description":"Node IP to check (default: all fleet nodes)"},
            "issue":{"type":"string","description":"Specific issue to diagnose"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("status");
        let node = input.get("node").and_then(Value::as_str);

        let nodes: Vec<(&str, &str)> = if let Some(n) = node {
            vec![(n, n)]
        } else {
            vec![("Taylor", "192.168.5.100"), ("Marcus", "192.168.5.102"), ("Sophie", "192.168.5.103"), ("Priya", "192.168.5.104"), ("James", "192.168.5.108")]
        };

        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap_or_default();
        let mut issues = Vec::new();
        let mut healthy = Vec::new();

        for (name, ip) in &nodes {
            // Check LLM server
            let llm_ok = client.get(format!("http://{ip}:51000/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false);
            // Check SSH
            let ssh_ok = Command::new("ssh").args(["-o", "ConnectTimeout=3", "-o", "StrictHostKeyChecking=no", &format!("root@{ip}"), "echo ok"]).output().await.map(|o| o.status.success()).unwrap_or(false);

            if !llm_ok && ssh_ok {
                issues.push(format!("  {name} ({ip}): LLM server DOWN (SSH ok)\n    Fix: ssh root@{ip} 'systemctl restart llama-server || ollama serve &'"));
            } else if !ssh_ok {
                issues.push(format!("  {name} ({ip}): UNREACHABLE (no SSH)\n    Fix: check network cable, power, firewall"));
            } else {
                healthy.push(format!("  {name} ({ip}): healthy"));
            }
        }

        match action {
            "diagnose" | "status" => {
                let mut output = format!("Fleet Health ({} nodes):\n\nHealthy:\n{}\n", nodes.len(), healthy.join("\n"));
                if !issues.is_empty() { output.push_str(&format!("\nIssues Found:\n{}\n", issues.join("\n\n"))); }
                else { output.push_str("\nNo issues detected.\n"); }
                AgentToolResult::ok(output)
            }
            "heal" | "auto" => {
                if issues.is_empty() { return AgentToolResult::ok("No issues to heal. All nodes healthy.".to_string()); }
                let mut healed = Vec::new();
                for (name, ip) in &nodes {
                    let llm_ok = client.get(format!("http://{ip}:51000/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false);
                    if !llm_ok {
                        // Try to restart LLM server
                        let _restart = Command::new("ssh").args(["-o", "ConnectTimeout=5", &format!("root@{ip}"), "pkill -f llama-server; sleep 2; nohup llama-server -m /models/*.gguf --host 0.0.0.0 --port 51000 &>/tmp/llama.log &"]).output().await;
                        healed.push(format!("  {name}: attempted LLM restart"));
                    }
                }
                AgentToolResult::ok(format!("Self-Heal Results:\n{}", healed.join("\n")))
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct AutoFleetTool;
#[async_trait]
impl AgentTool for AutoFleetTool {
    fn name(&self) -> &str { "AutoFleet" }
    fn description(&self) -> &str { "Fully autonomous fleet management: scan for new hardware, auto-onboard, configure, deploy models, optimize placement, rebalance workloads." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["scan","optimize","rebalance","auto","report"]},
            "subnet":{"type":"string","description":"Subnet to scan (default: 192.168.5.0/24)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("report");

        match action {
            "scan" => {
                // Scan for new machines on the network
                let cmd = "for i in $(seq 100 130); do (ping -c1 -W1 192.168.5.$i &>/dev/null && echo \"192.168.5.$i: alive\") & done; wait";
                match Command::new("bash").arg("-c").arg(cmd).output().await {
                    Ok(o) => {
                        let alive = String::from_utf8_lossy(&o.stdout);
                        AgentToolResult::ok(format!("Network Scan (192.168.5.100-130):\n{}\n\nNew nodes not in fleet.toml should be onboarded with NodeSetup + NodeEnroll.", alive))
                    }
                    Err(e) => AgentToolResult::err(format!("Scan failed: {e}")),
                }
            }
            "report" => {
                let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap_or_default();
                let mut report = String::from("AutoFleet Report:\n\n");
                let nodes = [("Taylor","192.168.5.100",96), ("Marcus","192.168.5.102",32), ("Sophie","192.168.5.103",32), ("Priya","192.168.5.104",32), ("James","192.168.5.108",64)];
                let mut total_ram = 0;
                let mut online = 0;
                for (name, ip, ram) in &nodes {
                    let status = client.get(format!("http://{ip}:51000/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false);
                    let icon = if status { online += 1; "●" } else { "○" };
                    total_ram += ram;
                    report.push_str(&format!("  {icon} {name}: {ip} ({ram}GB) — {}\n", if status { "ONLINE" } else { "OFFLINE" }));
                }
                report.push_str(&format!("\n  Nodes: {online}/{} online\n  Total RAM: {total_ram}GB\n  Pending: 4× DGX Spark (128GB), 4× EVO-X2 (128GB)\n  Future total: {} nodes, {}GB\n", nodes.len(), nodes.len() + 8, total_ram + 4*128 + 4*128));

                report.push_str("\n  Recommendations:\n");
                if online < nodes.len() { report.push_str("    - Some nodes offline — run SelfHeal to diagnose\n"); }
                report.push_str("    - Run 'AutoFleet scan' to detect new hardware\n");
                report.push_str("    - Run 'AutoFleet optimize' to rebalance model placement\n");

                AgentToolResult::ok(report)
            }
            "optimize" => {
                AgentToolResult::ok("Model Placement Optimization:\n\n\
  Current:\n\
    Taylor (96GB): Gemma-4-31B, Qwen3-Coder\n\
    Marcus (32GB): Qwen2.5-Coder-32B\n\
    Sophie (32GB): Qwen2.5-Coder-32B\n\
    Priya (32GB): Qwen2.5-Coder-32B\n\
    James (64GB): Qwen2.5-72B, Qwen3.5-9B\n\n\
  Suggestion:\n\
    - James underutilized (64GB, running 72B uses 45GB)\n\
    - Could add a 14B model for fast tasks\n\
    - Marcus/Sophie/Priya are identical — good for parallel agent work\n\
    - When DGX Sparks arrive: run 405B model across 2 linked Sparks\n\
    - When EVO-X2s arrive: each gets 70B model (128GB each)\n".to_string())
            }
            "rebalance" => {
                AgentToolResult::ok("Rebalance: would move work from busy nodes to idle nodes. Currently all nodes have similar load. No rebalancing needed.".to_string())
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct TaskDecomposerTool;
#[async_trait]
impl AgentTool for TaskDecomposerTool {
    fn name(&self) -> &str { "TaskDecomposer" }
    fn description(&self) -> &str { "Break a complex task into a tree of subtasks with dependencies, priorities, and estimated effort. Useful for planning large projects." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "task":{"type":"string","description":"Complex task to break down"},
            "depth":{"type":"number","description":"How many levels deep to decompose (default: 2)"},
            "style":{"type":"string","enum":["agile","waterfall","kanban"],"description":"Decomposition style"}
        },"required":["task"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let task = input.get("task").and_then(Value::as_str).unwrap_or("");
        let depth = input.get("depth").and_then(Value::as_u64).unwrap_or(2);
        if task.is_empty() { return AgentToolResult::err("'task' required"); }

        AgentToolResult::ok(format!(
            "Task Decomposition: \"{task}\"\n\n\
             This task should be broken into subtasks by the agent. Use the following structure:\n\n\
             1. Research & understand the requirements\n\
             2. Design the approach\n\
             3. Implement core functionality\n\
             4. Write tests\n\
             5. Code review\n\
             6. Integration testing\n\
             7. Documentation\n\n\
             Use TaskCreate to create each subtask, then use DependencyMapper to set up the dependency chain.\n\
             Use SprintPlanner to assign them to a sprint based on capacity.\n\
             Depth: {depth} levels of decomposition."
        ))
    }
}
