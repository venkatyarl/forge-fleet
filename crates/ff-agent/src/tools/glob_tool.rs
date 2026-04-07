//! Glob tool — fast file pattern matching.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct GlobTool;

#[async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (e.g. '**/*.rs', 'src/**/*.ts'). Returns matching file paths sorted by modification time. Use this to find files by name patterns."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files against (e.g. '**/*.rs')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let pattern = match input.get("pattern").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return AgentToolResult::err("Missing or empty 'pattern' parameter"),
        };

        let base_dir = input
            .get("path")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let full_pattern = if pattern.starts_with('/') {
            pattern
        } else {
            format!("{}/{}", base_dir.display(), pattern)
        };

        // Run glob in a blocking task since it does filesystem I/O
        let result = tokio::task::spawn_blocking(move || {
            let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();

            let paths = match glob::glob(&full_pattern) {
                Ok(paths) => paths,
                Err(e) => return Err(format!("Invalid glob pattern: {e}")),
            };

            for entry in paths.take(500) {
                match entry {
                    Ok(path) => {
                        // Skip hidden directories and common noise
                        let path_str = path.display().to_string();
                        if should_skip(&path_str) {
                            continue;
                        }
                        let mtime = path
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        entries.push((path_str, mtime));
                    }
                    Err(_) => continue,
                }
            }

            // Sort by modification time, newest first
            entries.sort_by(|a, b| b.1.cmp(&a.1));

            // Limit to 250 results
            entries.truncate(250);

            Ok(entries.into_iter().map(|(p, _)| p).collect::<Vec<_>>())
        })
        .await;

        match result {
            Ok(Ok(paths)) => {
                if paths.is_empty() {
                    AgentToolResult::ok("No files found matching the pattern.")
                } else {
                    let count = paths.len();
                    let output = paths.join("\n");
                    AgentToolResult::ok(truncate_output(
                        &format!("{output}\n\n{count} files found"),
                        MAX_TOOL_RESULT_CHARS,
                    ))
                }
            }
            Ok(Err(e)) => AgentToolResult::err(e),
            Err(e) => AgentToolResult::err(format!("Glob task failed: {e}")),
        }
    }
}

fn should_skip(path: &str) -> bool {
    let skip_segments = [
        "/.git/",
        "/node_modules/",
        "/target/",
        "/__pycache__/",
        "/.next/",
        "/dist/",
        "/.DS_Store",
    ];
    skip_segments.iter().any(|seg| path.contains(seg))
}
