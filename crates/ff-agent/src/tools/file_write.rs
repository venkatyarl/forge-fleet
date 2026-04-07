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

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        // Create parent directories
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                return AgentToolResult::err(format!(
                    "Failed to create parent directories for {}: {e}",
                    path.display()
                ));
            }
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
