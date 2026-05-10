//! FileWrite tool — write content to a file, creating parent directories.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct FileWriteTool;

#[async_trait]
impl AgentTool for FileWriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, or overwrites if it does. Automatically creates parent directories."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => return AgentToolResult::err("Missing or empty 'file_path' parameter"),
        };

        let content = match input.get("content").and_then(Value::as_str) {
            Some(c) => c,
            None => return AgentToolResult::err("Missing 'content' parameter"),
        };

        let path = match resolve_path(file_path, &ctx.working_dir) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };

        // Create parent directories
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent).await
        {
            return AgentToolResult::err(format!(
                "Failed to create parent directories for {}: {e}",
                path.display()
            ));
        }

        match fs::write(&path, content).await {
            Ok(()) => {
                let lines = content.lines().count();
                let bytes = content.len();
                AgentToolResult::ok(format!(
                    "Successfully wrote to {} ({lines} lines, {bytes} bytes)",
                    path.display()
                ))
            }
            Err(e) => AgentToolResult::err(format!("Failed to write {}: {e}", path.display())),
        }
    }
}

/// Resolve a user-provided file path, sandboxing it to the working directory.
/// Blocks path traversal (`..`) and absolute paths outside the workspace.
fn resolve_path(
    file_path: &str,
    working_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let path = std::path::Path::new(file_path);

    // Block parent directory traversal attempts
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Path traversal ('..') is not allowed".to_string());
    }

    let resolved = if path.is_absolute() {
        if !path.starts_with(working_dir) {
            return Err("Absolute path must be within the working directory".to_string());
        }
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };

    Ok(resolved)
}
