//! Code quality tools — complexity analysis, duplicate detection, API doc validation.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct CodeComplexityTool;
#[async_trait]
impl AgentTool for CodeComplexityTool {
    fn name(&self) -> &str {
        "CodeComplexity"
    }
    fn description(&self) -> &str {
        "Analyze code complexity: function count, line counts, nesting depth, and file size distribution."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"Directory or file to analyze (default: current dir)"},"language":{"type":"string","description":"Filter by language extension (e.g. 'rs', 'ts')"}}})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let lang = input.get("language").and_then(Value::as_str);
        let target = if path == "." {
            ctx.working_dir.clone()
        } else if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            ctx.working_dir.join(path)
        };

        // Use cloc or wc as fallback
        let cloc_result = Command::new("cloc")
            .arg("--quiet")
            .arg(&target)
            .output()
            .await;
        match cloc_result {
            Ok(out) if out.status.success() => AgentToolResult::ok(format!(
                "Code Complexity Analysis:\n\n{}",
                truncate_output(
                    &String::from_utf8_lossy(&out.stdout),
                    MAX_TOOL_RESULT_CHARS - 100
                )
            )),
            _ => {
                // Fallback: basic file stats
                let ext_filter = lang
                    .map(|l| format!("*.{l}"))
                    .unwrap_or_else(|| "*".to_string());
                let cmd = format!(
                    "find '{}' -name '{}' -not -path '*/target/*' -not -path '*/node_modules/*' -not -path '*/.git/*' | head -100 | xargs wc -l 2>/dev/null | sort -rn | head -20",
                    target.display(),
                    ext_filter
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(out) => AgentToolResult::ok(format!(
                        "File size analysis (lines):\n\n{}",
                        truncate_output(
                            &String::from_utf8_lossy(&out.stdout),
                            MAX_TOOL_RESULT_CHARS - 100
                        )
                    )),
                    Err(e) => AgentToolResult::err(format!("Analysis failed: {e}")),
                }
            }
        }
    }
}

pub struct DuplicateDetectorTool;
#[async_trait]
impl AgentTool for DuplicateDetectorTool {
    fn name(&self) -> &str {
        "DuplicateDetector"
    }
    fn description(&self) -> &str {
        "Find duplicate or near-duplicate code blocks in a codebase. Identifies copy-paste patterns."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"Directory to scan"},"min_lines":{"type":"number","description":"Minimum duplicate block size (default: 6 lines)"},"language":{"type":"string","description":"File extension filter"}}})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let min_lines = input.get("min_lines").and_then(Value::as_u64).unwrap_or(6);
        let target = if path == "." {
            ctx.working_dir.clone()
        } else {
            ctx.working_dir.join(path)
        };

        // Try jscpd (JavaScript Copy/Paste Detector) or report that it's needed
        let result = Command::new("npx")
            .args([
                "jscpd",
                "--min-lines",
                &min_lines.to_string(),
                "--reporters",
                "console",
                &target.to_string_lossy(),
            ])
            .output()
            .await;
        match result {
            Ok(out) if out.status.success() => AgentToolResult::ok(truncate_output(
                &String::from_utf8_lossy(&out.stdout),
                MAX_TOOL_RESULT_CHARS,
            )),
            _ => {
                // Fallback: basic duplicate line detection using sort | uniq -d
                let lang = input
                    .get("language")
                    .and_then(Value::as_str)
                    .unwrap_or("rs");
                let cmd = format!(
                    "find '{}' -name '*.{}' -not -path '*/target/*' | xargs cat 2>/dev/null | sed 's/^[[:space:]]*//' | sort | uniq -cd | sort -rn | head -20",
                    target.display(),
                    lang
                );
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(out) => {
                        let output = String::from_utf8_lossy(&out.stdout);
                        if output.trim().is_empty() {
                            AgentToolResult::ok("No significant duplicates found.".to_string())
                        } else {
                            AgentToolResult::ok(format!(
                                "Most repeated lines:\n\n{}",
                                truncate_output(&output, MAX_TOOL_RESULT_CHARS - 100)
                            ))
                        }
                    }
                    Err(e) => AgentToolResult::err(format!("Duplicate detection failed: {e}")),
                }
            }
        }
    }
}

pub struct LogAnalyzerTool;
#[async_trait]
impl AgentTool for LogAnalyzerTool {
    fn name(&self) -> &str {
        "LogAnalyzer"
    }
    fn description(&self) -> &str {
        "Parse and analyze log files. Find errors, warnings, patterns, and frequency distributions."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"file_path":{"type":"string","description":"Path to log file"},"filter":{"type":"string","description":"Filter pattern (e.g. 'ERROR', 'WARN')"},"tail":{"type":"number","description":"Number of recent lines to analyze (default: 500)"}},"required":["file_path"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        let filter = input.get("filter").and_then(Value::as_str).unwrap_or("");
        let tail = input.get("tail").and_then(Value::as_u64).unwrap_or(500);

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };
        if !path.exists() {
            return AgentToolResult::err(format!("File not found: {}", path.display()));
        }

        let cmd = if filter.is_empty() {
            format!(
                "tail -n {} '{}' | sort | uniq -c | sort -rn | head -30",
                tail,
                path.display()
            )
        } else {
            format!(
                "tail -n {} '{}' | grep -i '{}' | head -50",
                tail,
                path.display(),
                filter
            )
        };

        match Command::new("bash").arg("-c").arg(&cmd).output().await {
            Ok(out) => {
                let output = String::from_utf8_lossy(&out.stdout);
                // Count errors and warnings
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(_) => String::new(),
                };
                let lines = content.lines().count();
                let errors = content
                    .lines()
                    .filter(|l| l.to_ascii_lowercase().contains("error"))
                    .count();
                let warns = content
                    .lines()
                    .filter(|l| l.to_ascii_lowercase().contains("warn"))
                    .count();

                AgentToolResult::ok(format!(
                    "Log Analysis: {}\n  Total lines: {lines}\n  Errors: {errors}\n  Warnings: {warns}\n\n{}",
                    path.display(),
                    if filter.is_empty() {
                        format!("Most frequent patterns:\n{output}")
                    } else {
                        format!("Filtered ({filter}):\n{output}")
                    }
                ))
            }
            Err(e) => AgentToolResult::err(format!("Log analysis failed: {e}")),
        }
    }
}
