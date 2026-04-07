//! JsonQuery tool — query and transform JSON data.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct JsonQueryTool;

#[async_trait]
impl AgentTool for JsonQueryTool {
    fn name(&self) -> &str { "JsonQuery" }

    fn description(&self) -> &str {
        "Query and transform JSON data using jq-style expressions. Can process JSON from files, strings, or URLs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "jq expression (e.g. '.data[].name', '.[] | select(.status == \"active\")'" },
                "input": { "type": "string", "description": "JSON string to query" },
                "file": { "type": "string", "description": "Path to JSON file to query (alternative to input)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let query = match input.get("query").and_then(Value::as_str) {
            Some(q) => q,
            None => return AgentToolResult::err("Missing 'query'"),
        };

        // Get JSON input from string or file
        let json_input = if let Some(json_str) = input.get("input").and_then(Value::as_str) {
            json_str.to_string()
        } else if let Some(file_path) = input.get("file").and_then(Value::as_str) {
            let path = if std::path::Path::new(file_path).is_absolute() {
                std::path::PathBuf::from(file_path)
            } else {
                ctx.working_dir.join(file_path)
            };
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => content,
                Err(e) => return AgentToolResult::err(format!("Failed to read {}: {e}", path.display())),
            }
        } else {
            return AgentToolResult::err("Provide either 'input' (JSON string) or 'file' (path to JSON file)");
        };

        // Try using jq if available, otherwise basic Rust JSON parsing
        let jq_result = tokio::process::Command::new("jq")
            .arg(query)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match jq_result {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(json_input.as_bytes()).await;
                }
                match child.wait_with_output().await {
                    Ok(out) if out.status.success() => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        AgentToolResult::ok(truncate_output(&stdout, MAX_TOOL_RESULT_CHARS))
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        AgentToolResult::err(format!("jq error: {stderr}"))
                    }
                    Err(e) => AgentToolResult::err(format!("jq failed: {e}")),
                }
            }
            Err(_) => {
                // jq not available — basic key access
                match serde_json::from_str::<Value>(&json_input) {
                    Ok(data) => {
                        // Simple dot-notation access
                        let result = simple_json_query(&data, query);
                        AgentToolResult::ok(truncate_output(&serde_json::to_string_pretty(&result).unwrap_or_default(), MAX_TOOL_RESULT_CHARS))
                    }
                    Err(e) => AgentToolResult::err(format!("Invalid JSON input: {e}")),
                }
            }
        }
    }
}

fn simple_json_query(data: &Value, query: &str) -> Value {
    let path = query.trim_start_matches('.');
    if path.is_empty() { return data.clone(); }

    let mut current = data;
    for key in path.split('.') {
        if key.is_empty() { continue; }
        if let Some(idx) = key.strip_prefix('[').and_then(|k| k.strip_suffix(']')) {
            if let Ok(i) = idx.parse::<usize>() {
                current = current.get(i).unwrap_or(&Value::Null);
            }
        } else {
            current = current.get(key).unwrap_or(&Value::Null);
        }
    }
    current.clone()
}
