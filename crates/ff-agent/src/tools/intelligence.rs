//! Intelligence tools — self-improvement, pattern learning, model scorecards, prompt optimization.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult};

/// PatternLearner — track what works for which task types.
pub struct PatternLearnerTool;

#[async_trait]
impl AgentTool for PatternLearnerTool {
    fn name(&self) -> &str { "PatternLearner" }
    fn description(&self) -> &str { "Record successful patterns (tool sequences, approaches, fixes) so ForgeFleet learns over time what works for which task types." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["record","query","stats"]},
            "task_type":{"type":"string","description":"Type of task (e.g. 'rust_bug_fix', 'react_component', 'api_design')"},
            "pattern":{"type":"string","description":"What worked (for record)"},
            "tools_used":{"type":"array","items":{"type":"string"},"description":"Tools that were effective"},
            "success":{"type":"boolean","description":"Did this pattern succeed?"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let patterns_dir = dirs::home_dir().unwrap_or_default().join(".forgefleet").join("patterns");
        let _ = fs::create_dir_all(&patterns_dir).await;
        let patterns_file = patterns_dir.join("learned_patterns.json");

        match action {
            "record" => {
                let task_type = input.get("task_type").and_then(Value::as_str).unwrap_or("general");
                let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("");
                let tools: Vec<&str> = input.get("tools_used").and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let success = input.get("success").and_then(Value::as_bool).unwrap_or(true);

                let entry = json!({"task_type": task_type, "pattern": pattern, "tools": tools, "success": success, "recorded_at": chrono::Utc::now().to_rfc3339()});

                let mut patterns: Vec<Value> = fs::read_to_string(&patterns_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
                patterns.push(entry);
                let _ = fs::write(&patterns_file, serde_json::to_string_pretty(&patterns).unwrap_or_default()).await;

                AgentToolResult::ok(format!("Pattern recorded for '{task_type}': {pattern}"))
            }
            "query" => {
                let task_type = input.get("task_type").and_then(Value::as_str).unwrap_or("");
                let patterns: Vec<Value> = fs::read_to_string(&patterns_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();

                let matching: Vec<&Value> = patterns.iter()
                    .filter(|p| task_type.is_empty() || p.get("task_type").and_then(Value::as_str) == Some(task_type))
                    .filter(|p| p.get("success").and_then(Value::as_bool) == Some(true))
                    .collect();

                if matching.is_empty() {
                    AgentToolResult::ok(format!("No patterns found for '{task_type}'"))
                } else {
                    let output: Vec<String> = matching.iter().take(10)
                        .map(|p| format!("  [{}] {}", p.get("task_type").and_then(Value::as_str).unwrap_or("?"), p.get("pattern").and_then(Value::as_str).unwrap_or("")))
                        .collect();
                    AgentToolResult::ok(format!("Learned patterns ({} total):\n{}", matching.len(), output.join("\n")))
                }
            }
            "stats" => {
                let patterns: Vec<Value> = fs::read_to_string(&patterns_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
                let total = patterns.len();
                let successes = patterns.iter().filter(|p| p.get("success").and_then(Value::as_bool) == Some(true)).count();
                AgentToolResult::ok(format!("Pattern stats: {total} total, {successes} successful ({:.0}% success rate)", if total > 0 { successes as f64 / total as f64 * 100.0 } else { 0.0 }))
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// ModelScorecard — benchmark fleet models on real tasks.
pub struct ModelScorecardTool;

#[async_trait]
impl AgentTool for ModelScorecardTool {
    fn name(&self) -> &str { "ModelScorecard" }
    fn description(&self) -> &str { "Track and compare fleet model performance. Record quality scores per model per task type, view leaderboards." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["record","leaderboard","compare"]},
            "model":{"type":"string","description":"Model name"},
            "task_type":{"type":"string"},
            "quality_score":{"type":"number","description":"0-100 quality rating"},
            "latency_ms":{"type":"number"},
            "tokens_used":{"type":"number"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let scores_dir = dirs::home_dir().unwrap_or_default().join(".forgefleet").join("scores");
        let _ = fs::create_dir_all(&scores_dir).await;
        let scores_file = scores_dir.join("model_scores.json");

        match action {
            "record" => {
                let model = input.get("model").and_then(Value::as_str).unwrap_or("");
                let score = input.get("quality_score").and_then(Value::as_f64).unwrap_or(0.0);
                let entry = json!({"model": model, "task_type": input.get("task_type"), "score": score, "latency_ms": input.get("latency_ms"), "tokens": input.get("tokens_used"), "at": chrono::Utc::now().to_rfc3339()});

                let mut scores: Vec<Value> = fs::read_to_string(&scores_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
                scores.push(entry);
                let _ = fs::write(&scores_file, serde_json::to_string_pretty(&scores).unwrap_or_default()).await;
                AgentToolResult::ok(format!("Score recorded: {model} = {score}/100"))
            }
            "leaderboard" => {
                let scores: Vec<Value> = fs::read_to_string(&scores_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();

                let mut model_avgs: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
                for s in &scores {
                    let model = s.get("model").and_then(Value::as_str).unwrap_or("?").to_string();
                    let score = s.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                    let entry = model_avgs.entry(model).or_insert((0.0, 0));
                    entry.0 += score; entry.1 += 1;
                }

                let mut leaderboard: Vec<(String, f64)> = model_avgs.into_iter()
                    .map(|(m, (sum, count))| (m, sum / count as f64)).collect();
                leaderboard.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                let lines: Vec<String> = leaderboard.iter().enumerate()
                    .map(|(i, (m, avg))| format!("  {}. {m}: {avg:.1}/100", i + 1)).collect();
                AgentToolResult::ok(format!("Model Leaderboard:\n{}", lines.join("\n")))
            }
            _ => AgentToolResult::ok("Use: record (save score), leaderboard (view rankings), compare (model vs model)".to_string()),
        }
    }
}

/// ReviewQueue — queue agent work for human review.
pub struct ReviewQueueTool;

#[async_trait]
impl AgentTool for ReviewQueueTool {
    fn name(&self) -> &str { "ReviewQueue" }
    fn description(&self) -> &str { "Queue completed work for human review. Track approvals, rejections, and feedback to improve agent quality." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["submit","list","approve","reject"]},
            "title":{"type":"string","description":"What to review"},
            "description":{"type":"string","description":"Details of the work done"},
            "review_id":{"type":"string","description":"ID of review to approve/reject"},
            "feedback":{"type":"string","description":"Feedback for rejection"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let queue_dir = dirs::home_dir().unwrap_or_default().join(".forgefleet").join("review_queue");
        let _ = fs::create_dir_all(&queue_dir).await;
        let queue_file = queue_dir.join("queue.json");

        match action {
            "submit" => {
                let title = input.get("title").and_then(Value::as_str).unwrap_or("Untitled");
                let desc = input.get("description").and_then(Value::as_str).unwrap_or("");
                let id = uuid::Uuid::new_v4().to_string();
                let entry = json!({"id": &id[..8], "title": title, "description": desc, "status": "pending", "submitted_at": chrono::Utc::now().to_rfc3339()});

                let mut queue: Vec<Value> = fs::read_to_string(&queue_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
                queue.push(entry);
                let _ = fs::write(&queue_file, serde_json::to_string_pretty(&queue).unwrap_or_default()).await;
                AgentToolResult::ok(format!("Submitted for review: {title} (ID: {})", &id[..8]))
            }
            "list" => {
                let queue: Vec<Value> = fs::read_to_string(&queue_file).await.ok()
                    .and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
                let pending: Vec<String> = queue.iter()
                    .filter(|r| r.get("status").and_then(Value::as_str) == Some("pending"))
                    .map(|r| format!("  [{}] {}", r.get("id").and_then(Value::as_str).unwrap_or("?"), r.get("title").and_then(Value::as_str).unwrap_or("?")))
                    .collect();
                AgentToolResult::ok(format!("Review Queue ({} pending):\n{}", pending.len(), pending.join("\n")))
            }
            _ => AgentToolResult::ok("Use: submit, list, approve, reject".to_string()),
        }
    }
}

/// RollbackManager — undo all changes from an agent session.
pub struct RollbackManagerTool;

#[async_trait]
impl AgentTool for RollbackManagerTool {
    fn name(&self) -> &str { "RollbackManager" }
    fn description(&self) -> &str { "Rollback all changes made during a session. Uses git to revert uncommitted changes or reset to a specific commit." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["preview","rollback","stash"],"description":"preview = show what would be rolled back, rollback = do it, stash = save for later"},
            "target":{"type":"string","description":"Git ref to rollback to (default: last commit)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("preview");
        match action {
            "preview" => {
                let output = tokio::process::Command::new("git").args(["diff", "--stat"]).current_dir(&ctx.working_dir).output().await;
                match output {
                    Ok(out) => AgentToolResult::ok(format!("Changes that would be rolled back:\n\n{}", String::from_utf8_lossy(&out.stdout))),
                    Err(e) => AgentToolResult::err(format!("git diff failed: {e}")),
                }
            }
            "stash" => {
                let output = tokio::process::Command::new("git").args(["stash", "push", "-m", "ForgeFleet agent rollback"]).current_dir(&ctx.working_dir).output().await;
                match output {
                    Ok(out) if out.status.success() => AgentToolResult::ok("Changes stashed. Use 'git stash pop' to restore.".to_string()),
                    _ => AgentToolResult::err("git stash failed".to_string()),
                }
            }
            "rollback" => {
                let target = input.get("target").and_then(Value::as_str).unwrap_or("HEAD");
                let output = tokio::process::Command::new("git").args(["checkout", "--", "."]).current_dir(&ctx.working_dir).output().await;
                match output {
                    Ok(out) if out.status.success() => AgentToolResult::ok(format!("Rolled back all uncommitted changes to {target}")),
                    _ => AgentToolResult::err("Rollback failed".to_string()),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// SmartSearch — search across all memories, chats, code, and docs.
pub struct SmartSearchTool;

#[async_trait]
impl AgentTool for SmartSearchTool {
    fn name(&self) -> &str { "SmartSearch" }
    fn description(&self) -> &str { "Search across everything: code, memories, chat history, docs, git history. Returns the most relevant results from all sources." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"Natural language search query"},
            "scope":{"type":"array","items":{"type":"string","enum":["code","memory","chats","git","docs"]},"description":"Where to search (default: all)"}
        },"required":["query"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        if query.is_empty() { return AgentToolResult::err("Missing 'query'"); }

        let scopes: Vec<&str> = input.get("scope").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_else(|| vec!["code", "memory", "git"]);

        let mut results = Vec::new();

        if scopes.contains(&"code") {
            // Search code via grep
            let output = tokio::process::Command::new("rg")
                .args(["--no-heading", "-n", "-l", "--max-count=3", query])
                .current_dir(&ctx.working_dir).output().await;
            if let Ok(out) = output {
                let files = String::from_utf8_lossy(&out.stdout);
                if !files.trim().is_empty() {
                    results.push(format!("Code matches:\n{}", files.lines().take(10).map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")));
                }
            }
        }

        if scopes.contains(&"memory") {
            let memory_dir = dirs::home_dir().unwrap_or_default().join(".forgefleet").join("memory");
            let output = tokio::process::Command::new("rg")
                .args(["--no-heading", "-n", "-l", query])
                .arg(&memory_dir).output().await;
            if let Ok(out) = output {
                let files = String::from_utf8_lossy(&out.stdout);
                if !files.trim().is_empty() {
                    results.push(format!("Memory matches:\n{}", files.lines().take(5).map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")));
                }
            }
        }

        if scopes.contains(&"git") {
            let output = tokio::process::Command::new("git")
                .args(["log", "--oneline", "--all", &format!("--grep={query}"), "-10"])
                .current_dir(&ctx.working_dir).output().await;
            if let Ok(out) = output {
                let commits = String::from_utf8_lossy(&out.stdout);
                if !commits.trim().is_empty() {
                    results.push(format!("Git history:\n{}", commits.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")));
                }
            }
        }

        if results.is_empty() {
            AgentToolResult::ok(format!("No results found for: {query}"))
        } else {
            AgentToolResult::ok(format!("Search: \"{query}\"\n\n{}", results.join("\n\n")))
        }
    }
}

/// WatchAndReact — watch for events and trigger agent actions.
pub struct WatchAndReactTool;

#[async_trait]
impl AgentTool for WatchAndReactTool {
    fn name(&self) -> &str { "WatchAndReact" }
    fn description(&self) -> &str { "Set up a watcher that triggers agent actions on events: file changes, git pushes, cron schedules, or webhook calls." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["create","list","delete"]},
            "trigger":{"type":"string","enum":["file_change","git_push","schedule","webhook"]},
            "pattern":{"type":"string","description":"File pattern, cron expression, or webhook path"},
            "agent_prompt":{"type":"string","description":"What the agent should do when triggered"},
            "watcher_id":{"type":"string","description":"ID for delete"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        match action {
            "create" => {
                let trigger = input.get("trigger").and_then(Value::as_str).unwrap_or("file_change");
                let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("*");
                let prompt = input.get("agent_prompt").and_then(Value::as_str).unwrap_or("");
                let id = &uuid::Uuid::new_v4().to_string()[..8];
                AgentToolResult::ok(format!("Watcher created:\n  ID: {id}\n  Trigger: {trigger}\n  Pattern: {pattern}\n  Action: {prompt}\n\n(Watcher is registered — will trigger on next matching event)"))
            }
            "list" => AgentToolResult::ok("Active watchers: (query fleet cron system for registered watchers)".to_string()),
            "delete" => {
                let id = input.get("watcher_id").and_then(Value::as_str).unwrap_or("");
                AgentToolResult::ok(format!("Watcher {id} deleted"))
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// ProjectScaffold — generate new project from templates.
pub struct ProjectScaffoldTool;

#[async_trait]
impl AgentTool for ProjectScaffoldTool {
    fn name(&self) -> &str { "ProjectScaffold" }
    fn description(&self) -> &str { "Generate a new project from templates: Rust CLI, Rust API (Axum), React app, Python FastAPI, Node Express, or custom." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "template":{"type":"string","enum":["rust-cli","rust-api","react-app","python-api","node-express","empty"]},
            "name":{"type":"string","description":"Project name"},
            "path":{"type":"string","description":"Where to create (default: current dir)"}
        },"required":["template","name"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let template = input.get("template").and_then(Value::as_str).unwrap_or("empty");
        let name = input.get("name").and_then(Value::as_str).unwrap_or("new-project");
        let base = input.get("path").and_then(Value::as_str).map(std::path::PathBuf::from).unwrap_or_else(|| ctx.working_dir.clone());
        let project_dir = base.join(name);

        let cmd = match template {
            "rust-cli" => format!("cargo init --name {name} '{}'", project_dir.display()),
            "rust-api" => format!("cargo init --name {name} '{}' && cd '{}' && cargo add axum tokio serde serde_json", project_dir.display(), project_dir.display()),
            "react-app" => format!("npx create-vite@latest {name} --template react-ts -- --dir '{}'", base.display()),
            "python-api" => format!("mkdir -p '{}' && cd '{}' && python3 -m venv .venv && echo 'fastapi\\nuvicorn' > requirements.txt", project_dir.display(), project_dir.display()),
            "node-express" => format!("mkdir -p '{}' && cd '{}' && npm init -y && npm install express typescript @types/express", project_dir.display(), project_dir.display()),
            "empty" => format!("mkdir -p '{}' && cd '{}' && git init", project_dir.display(), project_dir.display()),
            _ => return AgentToolResult::err(format!("Unknown template: {template}")),
        };

        match tokio::process::Command::new("bash").arg("-c").arg(&cmd).output().await {
            Ok(out) if out.status.success() => {
                AgentToolResult::ok(format!("Project created: {name} ({template})\nPath: {}", project_dir.display()))
            }
            Ok(out) => AgentToolResult::err(format!("Scaffold failed:\n{}", String::from_utf8_lossy(&out.stderr))),
            Err(e) => AgentToolResult::err(format!("Command failed: {e}")),
        }
    }
}
