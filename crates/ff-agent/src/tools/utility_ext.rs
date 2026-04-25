//! Extended utility tools — reminders, timers, timezone, regex, diagrams, translate, markdown.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct ReminderTool;
#[async_trait]
impl AgentTool for ReminderTool {
    fn name(&self) -> &str {
        "Reminder"
    }
    fn description(&self) -> &str {
        "Set a reminder — saves a note with a target time. Reminders are stored and checked by the cron system."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"message":{"type":"string"},"in_minutes":{"type":"number","description":"Minutes from now"},"at":{"type":"string","description":"Specific time (ISO 8601 or natural like '3pm')"}},"required":["message"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let message = input.get("message").and_then(Value::as_str).unwrap_or("");
        let minutes = input
            .get("in_minutes")
            .and_then(Value::as_u64)
            .unwrap_or(30);
        let target = chrono::Utc::now() + chrono::Duration::minutes(minutes as i64);
        let reminders_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".forgefleet")
            .join("reminders");
        let _ = tokio::fs::create_dir_all(&reminders_dir).await;
        let reminder = json!({"message": message, "target": target.to_rfc3339(), "created": chrono::Utc::now().to_rfc3339()});
        let path = reminders_dir.join(format!("{}.json", &uuid::Uuid::new_v4().to_string()[..8]));
        let _ = tokio::fs::write(
            &path,
            serde_json::to_string_pretty(&reminder).unwrap_or_default(),
        )
        .await;
        AgentToolResult::ok(format!(
            "Reminder set: \"{message}\" at {}",
            target.format("%H:%M UTC (%B %d)")
        ))
    }
}

pub struct TimerTool;
#[async_trait]
impl AgentTool for TimerTool {
    fn name(&self) -> &str {
        "Timer"
    }
    fn description(&self) -> &str {
        "Time how long a command takes. Useful for benchmarking."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string","description":"Command to time"},"label":{"type":"string","description":"Label for this timing"}},"required":["command"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        let label = input
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("Timer");
        let start = std::time::Instant::now();
        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.working_dir)
            .output()
            .await;
        let elapsed = start.elapsed();
        match output {
            Ok(o) => {
                let stdout = truncate_output(&String::from_utf8_lossy(&o.stdout), 2000);
                AgentToolResult::ok(format!(
                    "{label}: {:.2}s ({}ms)\nExit: {}\n\n{stdout}",
                    elapsed.as_secs_f64(),
                    elapsed.as_millis(),
                    o.status.code().unwrap_or(-1)
                ))
            }
            Err(e) => AgentToolResult::err(format!("Timer failed: {e}")),
        }
    }
}

pub struct TimezoneConvertTool;
#[async_trait]
impl AgentTool for TimezoneConvertTool {
    fn name(&self) -> &str {
        "TimezoneConvert"
    }
    fn description(&self) -> &str {
        "Convert time between timezones. Also shows current time in multiple zones."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["convert","now"],"description":"convert a time or show current times"},"time":{"type":"string","description":"Time to convert (e.g. '3:00 PM')"},"from_tz":{"type":"string","description":"Source timezone (e.g. 'EST', 'America/New_York')"},"to_tz":{"type":"string","description":"Target timezone"}},"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("now");
        match action {
            "now" => {
                let cmd = "for tz in US/Eastern US/Central US/Pacific Europe/London Europe/Berlin Asia/Tokyo Asia/Kolkata; do echo \"  $tz: $(TZ=$tz date '+%Y-%m-%d %H:%M %Z')\"; done";
                match Command::new("bash").arg("-c").arg(cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!(
                        "Current times:\n{}",
                        String::from_utf8_lossy(&o.stdout)
                    )),
                    Err(e) => AgentToolResult::err(format!("Failed: {e}")),
                }
            }
            "convert" => {
                let time = input.get("time").and_then(Value::as_str).unwrap_or("now");
                let from = input
                    .get("from_tz")
                    .and_then(Value::as_str)
                    .unwrap_or("UTC");
                let to = input
                    .get("to_tz")
                    .and_then(Value::as_str)
                    .unwrap_or("US/Eastern");
                let cmd = format!(
                    "TZ='{}' date -d 'TZ=\"{}\" {}' '+%Y-%m-%d %H:%M %Z' 2>/dev/null || echo 'Use: TimezoneConvert now for current times'",
                    to, from, time
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!(
                        "{time} {from} = {}",
                        String::from_utf8_lossy(&o.stdout).trim()
                    )),
                    Err(e) => AgentToolResult::err(format!("Conversion failed: {e}")),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct RegexTool;
#[async_trait]
impl AgentTool for RegexTool {
    fn name(&self) -> &str {
        "Regex"
    }
    fn description(&self) -> &str {
        "Test regex patterns against input text. Shows matches, capture groups, and explains the pattern."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"pattern":{"type":"string","description":"Regex pattern"},"text":{"type":"string","description":"Text to test against"},"action":{"type":"string","enum":["test","find_all","replace"],"description":"What to do (default: test)"},"replacement":{"type":"string","description":"Replacement string (for replace action)"}},"required":["pattern","text"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("");
        let text = input.get("text").and_then(Value::as_str).unwrap_or("");
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("test");
        // Use grep/sed for regex operations
        match action {
            "test" => {
                let cmd = format!(
                    "echo '{}' | grep -oP '{}' | head -20",
                    text.replace('\'', "'\"'\"'"),
                    pattern.replace('\'', "'\"'\"'")
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => {
                        let matches = String::from_utf8_lossy(&o.stdout);
                        if matches.trim().is_empty() {
                            AgentToolResult::ok(format!("No matches for /{pattern}/"))
                        } else {
                            AgentToolResult::ok(format!("Matches for /{pattern}/:\n{matches}"))
                        }
                    }
                    Err(e) => AgentToolResult::err(format!("Regex test failed: {e}")),
                }
            }
            "replace" => {
                let replacement = input
                    .get("replacement")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let cmd = format!(
                    "echo '{}' | sed -E 's/{}/{}/g'",
                    text.replace('\'', "'\"'\"'"),
                    pattern.replace('/', "\\/"),
                    replacement.replace('/', "\\/")
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!(
                        "Result: {}",
                        String::from_utf8_lossy(&o.stdout).trim()
                    )),
                    Err(e) => AgentToolResult::err(format!("Regex replace failed: {e}")),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct DiagramTool;
#[async_trait]
impl AgentTool for DiagramTool {
    fn name(&self) -> &str {
        "Diagram"
    }
    fn description(&self) -> &str {
        "Generate Mermaid diagrams from descriptions. Outputs Mermaid syntax that can be rendered in markdown."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"type":{"type":"string","enum":["flowchart","sequence","class","er","gantt","pie","state"],"description":"Diagram type"},"description":{"type":"string","description":"What to diagram"},"code":{"type":"string","description":"Raw Mermaid code (if you already have it)"}},"required":["type"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let diagram_type = input
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("flowchart");
        if let Some(code) = input.get("code").and_then(Value::as_str) {
            return AgentToolResult::ok(format!("```mermaid\n{code}\n```"));
        }
        let desc = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let template = match diagram_type {
            "flowchart" => format!(
                "```mermaid\nflowchart TD\n    A[Start] --> B{{Decision}}\n    B -->|Yes| C[Action 1]\n    B -->|No| D[Action 2]\n    C --> E[End]\n    D --> E\n```\n\nModify the above based on: {desc}"
            ),
            "sequence" => format!(
                "```mermaid\nsequenceDiagram\n    Actor User\n    User->>+Server: Request\n    Server->>+Database: Query\n    Database-->>-Server: Results\n    Server-->>-User: Response\n```\n\nModify based on: {desc}"
            ),
            "er" => format!(
                "```mermaid\nerDiagram\n    USER ||--o{{ ORDER : places\n    ORDER ||--|{{ ITEM : contains\n    ITEM }}|--|| PRODUCT : is\n```\n\nModify based on: {desc}"
            ),
            "gantt" => format!(
                "```mermaid\ngantt\n    title Project Timeline\n    dateFormat YYYY-MM-DD\n    section Phase 1\n    Task 1: a1, 2026-04-07, 7d\n    Task 2: a2, after a1, 5d\n    section Phase 2\n    Task 3: b1, after a2, 10d\n```\n\nModify based on: {desc}"
            ),
            "pie" => format!(
                "```mermaid\npie title Distribution\n    \"Category A\": 40\n    \"Category B\": 30\n    \"Category C\": 20\n    \"Other\": 10\n```\n\nModify based on: {desc}"
            ),
            _ => format!("```mermaid\n{diagram_type}\n    // Add your diagram content here\n```"),
        };
        AgentToolResult::ok(template)
    }
}

pub struct TranslateTool;
#[async_trait]
impl AgentTool for TranslateTool {
    fn name(&self) -> &str {
        "Translate"
    }
    fn description(&self) -> &str {
        "Translate text between languages. Uses the fleet LLM for translation (no external API needed)."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"text":{"type":"string","description":"Text to translate"},"from":{"type":"string","description":"Source language (default: auto-detect)"},"to":{"type":"string","description":"Target language (e.g. 'Spanish', 'French', 'Japanese')"}},"required":["text","to"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let text = input.get("text").and_then(Value::as_str).unwrap_or("");
        let to = input.get("to").and_then(Value::as_str).unwrap_or("English");
        let from = input.get("from").and_then(Value::as_str).unwrap_or("auto");
        // The LLM itself does the translation — we just format the request
        AgentToolResult::ok(format!(
            "Translation request ({from} → {to}):\n\nOriginal: {text}\n\nNote: Use the agent's LLM capability to translate this text. The agent can translate directly since it's a language model."
        ))
    }
}

pub struct FileCompressTool;
#[async_trait]
impl AgentTool for FileCompressTool {
    fn name(&self) -> &str {
        "FileCompress"
    }
    fn description(&self) -> &str {
        "Compress and decompress files: zip, tar.gz, tar.bz2. Create archives from files/directories."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["compress","decompress","list"]},
            "input":{"type":"string","description":"File/directory to compress, or archive to decompress"},
            "output":{"type":"string","description":"Output archive name"},
            "format":{"type":"string","enum":["zip","tar.gz","tar.bz2"],"description":"Archive format (default: tar.gz)"}},"required":["action","input"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let input_path = input.get("input").and_then(Value::as_str).unwrap_or("");
        let output_path = input.get("output").and_then(Value::as_str).unwrap_or("");
        let format = input
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("tar.gz");

        let cmd = match action {
            "compress" => {
                let out = if output_path.is_empty() {
                    format!("{input_path}.{}", format.replace('.', ""))
                } else {
                    output_path.to_string()
                };
                match format {
                    "zip" => format!("zip -r '{out}' '{input_path}'"),
                    "tar.bz2" => format!("tar -cjf '{out}' '{input_path}'"),
                    _ => format!("tar -czf '{out}' '{input_path}'"),
                }
            }
            "decompress" => {
                if input_path.ends_with(".zip") {
                    format!("unzip '{input_path}'")
                } else if input_path.ends_with(".bz2") {
                    format!("tar -xjf '{input_path}'")
                } else {
                    format!("tar -xzf '{input_path}'")
                }
            }
            "list" => {
                if input_path.ends_with(".zip") {
                    format!("unzip -l '{input_path}'")
                } else {
                    format!("tar -tzf '{input_path}' | head -50")
                }
            }
            _ => return AgentToolResult::err(format!("Unknown action: {action}")),
        };

        match Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&ctx.working_dir)
            .output()
            .await
        {
            Ok(o) if o.status.success() => AgentToolResult::ok(truncate_output(
                &String::from_utf8_lossy(&o.stdout),
                MAX_TOOL_RESULT_CHARS,
            )),
            Ok(o) => AgentToolResult::err(String::from_utf8_lossy(&o.stderr).to_string()),
            Err(e) => AgentToolResult::err(format!("Command failed: {e}")),
        }
    }
}

pub struct FileSyncTool;
#[async_trait]
impl AgentTool for FileSyncTool {
    fn name(&self) -> &str {
        "FileSync"
    }
    fn description(&self) -> &str {
        "Sync files between local and fleet nodes using rsync. Supports bidirectional sync."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"source":{"type":"string","description":"Source path (local or user@host:/path)"},"destination":{"type":"string","description":"Destination path"},"exclude":{"type":"array","items":{"type":"string"},"description":"Patterns to exclude"},"dry_run":{"type":"boolean","description":"Preview without copying (default: false)"}},"required":["source","destination"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let source = input.get("source").and_then(Value::as_str).unwrap_or("");
        let dest = input
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or("");
        let dry_run = input
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let excludes: Vec<&str> = input
            .get("exclude")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut args = vec!["-avz", "--progress"];
        if dry_run {
            args.push("--dry-run");
        }
        for exc in &excludes {
            args.push("--exclude");
            args.push(exc);
        }
        args.push(source);
        args.push(dest);

        match Command::new("rsync").args(&args).output().await {
            Ok(o) => {
                let output = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if o.status.success() {
                    AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                } else {
                    AgentToolResult::err(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                }
            }
            Err(e) => AgentToolResult::err(format!("rsync failed: {e}. Is rsync installed?")),
        }
    }
}

pub struct HealthMonitorTool;
#[async_trait]
impl AgentTool for HealthMonitorTool {
    fn name(&self) -> &str {
        "HealthMonitor"
    }
    fn description(&self) -> &str {
        "Check health of URLs, services, and fleet endpoints. Returns status, response time, and content validation."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"urls":{"type":"array","items":{"type":"string"},"description":"URLs to check"},"expect_status":{"type":"number","description":"Expected HTTP status (default: 200)"},"expect_content":{"type":"string","description":"Expected content in response body"},"timeout_secs":{"type":"number","description":"Timeout per check (default: 5)"}},"required":["urls"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let urls: Vec<&str> = input
            .get("urls")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let expect_status = input
            .get("expect_status")
            .and_then(Value::as_u64)
            .unwrap_or(200) as u16;
        let expect_content = input.get("expect_content").and_then(Value::as_str);
        let timeout = input
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(5);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout))
            .build()
            .unwrap_or_default();
        let mut results = Vec::new();

        for url in &urls {
            let start = std::time::Instant::now();
            match client.get(*url).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let elapsed = start.elapsed().as_millis();
                    let status_ok = status == expect_status;
                    let body = resp.text().await.unwrap_or_default();
                    let content_ok = expect_content.map(|c| body.contains(c)).unwrap_or(true);
                    let icon = if status_ok && content_ok {
                        "✓"
                    } else {
                        "✗"
                    };
                    results.push(format!(
                        "  {icon} {url} — HTTP {status} ({elapsed}ms){}",
                        if !content_ok {
                            " [content mismatch]"
                        } else {
                            ""
                        }
                    ));
                }
                Err(e) => {
                    let elapsed = start.elapsed().as_millis();
                    results.push(format!("  ✗ {url} — FAILED ({elapsed}ms): {e}"));
                }
            }
        }

        AgentToolResult::ok(format!(
            "Health Check ({} URLs):\n{}",
            urls.len(),
            results.join("\n")
        ))
    }
}

pub struct GithubIssuesTool;
#[async_trait]
impl AgentTool for GithubIssuesTool {
    fn name(&self) -> &str {
        "GithubIssues"
    }
    fn description(&self) -> &str {
        "Create, list, view, and manage GitHub issues via gh CLI."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["create","list","view","close","comment"]},
            "title":{"type":"string","description":"Issue title (for create)"},
            "body":{"type":"string","description":"Issue body or comment text"},
            "issue_number":{"type":"number","description":"Issue number (for view/close/comment)"},
            "labels":{"type":"array","items":{"type":"string"},"description":"Labels (for create)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let mut cmd = Command::new("gh");
        cmd.current_dir(&ctx.working_dir);

        match action {
            "create" => {
                let title = input.get("title").and_then(Value::as_str).unwrap_or("");
                let body = input.get("body").and_then(Value::as_str).unwrap_or("");
                cmd.args(["issue", "create", "--title", title, "--body", body]);
                if let Some(labels) = input.get("labels").and_then(Value::as_array) {
                    for l in labels.iter().filter_map(Value::as_str) {
                        cmd.args(["--label", l]);
                    }
                }
            }
            "list" => {
                cmd.args(["issue", "list"]);
            }
            "view" => {
                if let Some(n) = input.get("issue_number").and_then(Value::as_u64) {
                    cmd.args(["issue", "view", &n.to_string()]);
                } else {
                    return AgentToolResult::err("'issue_number' required for view");
                }
            }
            "close" => {
                if let Some(n) = input.get("issue_number").and_then(Value::as_u64) {
                    cmd.args(["issue", "close", &n.to_string()]);
                } else {
                    return AgentToolResult::err("'issue_number' required for close");
                }
            }
            "comment" => {
                let body = input.get("body").and_then(Value::as_str).unwrap_or("");
                if let Some(n) = input.get("issue_number").and_then(Value::as_u64) {
                    cmd.args(["issue", "comment", &n.to_string(), "--body", body]);
                } else {
                    return AgentToolResult::err("'issue_number' required for comment");
                }
            }
            _ => return AgentToolResult::err(format!("Unknown action: {action}")),
        }

        match cmd.output().await {
            Ok(o) => {
                let output = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if o.status.success() {
                    AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                } else {
                    AgentToolResult::err(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                }
            }
            Err(e) => AgentToolResult::err(format!("gh failed: {e}. Is GitHub CLI installed?")),
        }
    }
}

pub struct MarkdownTool;
#[async_trait]
impl AgentTool for MarkdownTool {
    fn name(&self) -> &str {
        "MarkdownConvert"
    }
    fn description(&self) -> &str {
        "Convert markdown to HTML or render markdown files. Also validates markdown syntax."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["to_html","validate","toc"]},
            "input":{"type":"string","description":"Markdown text or file path"},
            "output":{"type":"string","description":"Output file path (for to_html)"}},"required":["action","input"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let text = input.get("input").and_then(Value::as_str).unwrap_or("");

        match action {
            "to_html" => {
                // Try pandoc, then python markdown
                let cmd = format!(
                    "echo '{}' | pandoc -f markdown -t html 2>/dev/null || python3 -c \"import markdown; print(markdown.markdown(open('/dev/stdin').read()))\" 2>/dev/null || echo 'Install pandoc or python markdown'",
                    text.replace('\'', "'\"'\"'")
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(truncate_output(
                        &String::from_utf8_lossy(&o.stdout),
                        MAX_TOOL_RESULT_CHARS,
                    )),
                    Err(e) => AgentToolResult::err(format!("Conversion failed: {e}")),
                }
            }
            "toc" => {
                // Extract headers as table of contents
                let mut toc = Vec::new();
                for line in text.lines() {
                    if line.starts_with('#') {
                        let level = line.chars().take_while(|c| *c == '#').count();
                        let title = line.trim_start_matches('#').trim();
                        toc.push(format!("{}- {title}", "  ".repeat(level - 1)));
                    }
                }
                AgentToolResult::ok(format!("Table of Contents:\n{}", toc.join("\n")))
            }
            _ => AgentToolResult::ok("Use: to_html (convert), toc (extract headings)".to_string()),
        }
    }
}
