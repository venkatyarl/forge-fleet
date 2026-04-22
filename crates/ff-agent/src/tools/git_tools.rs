//! Git tools — git blame analysis, branch management, commit helpers.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

/// Git blame analysis tool — trace when/why code changed.
pub struct GitBlameTool;

#[async_trait]
impl AgentTool for GitBlameTool {
    fn name(&self) -> &str { "GitBlame" }

    fn description(&self) -> &str {
        "Analyze git blame for a file or line range to understand when and why code changed. Useful for investigating bugs and understanding code history."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to the file" },
                "start_line": { "type": "number", "description": "Start line (optional)" },
                "end_line": { "type": "number", "description": "End line (optional)" }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) => p,
            None => return AgentToolResult::err("Missing 'file_path'"),
        };

        let mut args = vec!["blame", "--line-porcelain"];

        let line_range;
        if let (Some(start), Some(end)) = (
            input.get("start_line").and_then(Value::as_u64),
            input.get("end_line").and_then(Value::as_u64),
        ) {
            line_range = format!("-L{start},{end}");
            args.push(&line_range);
        }

        args.push(file_path);

        let output = Command::new("git")
            .args(&args)
            .current_dir(&ctx.working_dir)
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // Parse porcelain format into readable summary
                let summary = parse_blame_porcelain(&stdout);
                AgentToolResult::ok(truncate_output(&summary, MAX_TOOL_RESULT_CHARS))
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                AgentToolResult::err(format!("git blame failed: {stderr}"))
            }
            Err(e) => AgentToolResult::err(format!("Failed to run git blame: {e}")),
        }
    }
}

fn parse_blame_porcelain(output: &str) -> String {
    let mut result = String::new();
    let mut current_author = String::new();
    let mut current_summary = String::new();
    let mut line_num = 0u32;

    for line in output.lines() {
        if line.starts_with("author ") {
            current_author = line["author ".len()..].to_string();
        } else if line.starts_with("summary ") {
            current_summary = line["summary ".len()..].to_string();
        } else if line.starts_with('\t') {
            line_num += 1;
            let code = &line[1..];
            if !current_author.is_empty() {
                let author_preview: String = current_author.chars().take(20).collect();
                let summary_preview: String = current_summary.chars().take(40).collect();
                result.push_str(&format!("{line_num:>4} | {:<20} | {:<40} | {code}\n",
                    author_preview, summary_preview));
            }
        }
    }

    if result.is_empty() {
        output.to_string()
    } else {
        result
    }
}

/// Test generation tool — generate tests for a function.
pub struct TestGenTool;

#[async_trait]
impl AgentTool for TestGenTool {
    fn name(&self) -> &str { "TestGen" }

    fn description(&self) -> &str {
        "Generate unit tests for a specific function or module. Analyzes the code and creates appropriate test cases."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to the file containing the function" },
                "function_name": { "type": "string", "description": "Name of the function to test" },
                "test_framework": { "type": "string", "description": "Test framework (default: auto-detect from project)" }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) => p,
            None => return AgentToolResult::err("Missing 'file_path'"),
        };

        // Read the source file
        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return AgentToolResult::err(format!("Failed to read {}: {e}", path.display())),
        };

        let function_name = input.get("function_name").and_then(Value::as_str).unwrap_or("");

        // Return the source code for the LLM to generate tests from
        // The agent loop will use this as context to write actual tests
        let relevant_code = if !function_name.is_empty() {
            // Extract the function and surrounding context
            let lines: Vec<&str> = content.lines().collect();
            let mut found = false;
            let mut start = 0;
            let mut depth = 0i32;
            let mut end = lines.len();

            for (i, line) in lines.iter().enumerate() {
                if line.contains(&format!("fn {function_name}")) || line.contains(&format!("def {function_name}")) || line.contains(&format!("function {function_name}")) {
                    start = i.saturating_sub(3); // include docstrings/decorators
                    found = true;
                }
                if found {
                    for ch in line.chars() {
                        if ch == '{' || ch == ':' { depth += 1; }
                        if ch == '}' { depth -= 1; }
                    }
                    if depth <= 0 && i > start + 1 {
                        end = i + 1;
                        break;
                    }
                }
            }

            if found {
                lines[start..end.min(lines.len())].join("\n")
            } else {
                content.clone()
            }
        } else {
            content.clone()
        };

        AgentToolResult::ok(format!(
            "Source code for test generation:\n\nFile: {file_path}\n\n```\n{}\n```\n\nGenerate comprehensive tests for this code. Include edge cases, error cases, and happy path tests.",
            truncate_output(&relevant_code, 8000)
        ))
    }
}
