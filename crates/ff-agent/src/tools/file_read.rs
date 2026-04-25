//! FileRead tool — read files with offset/limit and line numbers.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct FileReadTool;

#[async_trait]
impl AgentTool for FileReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the filesystem. Returns contents with line numbers. Supports offset and limit for reading specific sections of large files."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read"
                },
                "offset": {
                    "type": "number",
                    "description": "Line number to start reading from (0-based, default 0)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to read (default 2000)"
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => return AgentToolResult::err("Missing or empty 'file_path' parameter"),
        };

        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;

        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(2000) as usize;

        let path = resolve_path(file_path, &ctx.working_dir);

        // Check if file exists
        let metadata = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                return AgentToolResult::err(format!(
                    "File does not exist or cannot be accessed: {}\nError: {e}",
                    path.display()
                ));
            }
        };

        if metadata.is_dir() {
            return AgentToolResult::err(format!(
                "{} is a directory, not a file. Use Bash with 'ls' to list directory contents.",
                path.display()
            ));
        }

        // Check if binary
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if is_binary_extension(&ext) {
            return AgentToolResult::err(format!(
                "{} appears to be a binary file ({ext}). Cannot display binary content.",
                path.display()
            ));
        }

        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                return AgentToolResult::err(format!("Failed to read {}: {e}", path.display()));
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        if offset >= total_lines {
            return AgentToolResult::ok(format!(
                "File has {total_lines} lines. Offset {offset} is past the end."
            ));
        }

        let end = (offset + limit).min(total_lines);
        let selected = &lines[offset..end];

        let mut output = String::new();
        for (idx, line) in selected.iter().enumerate() {
            let line_num = offset + idx + 1;
            output.push_str(&format!("{line_num}\t{line}\n"));
        }

        if end < total_lines {
            output.push_str(&format!(
                "\n... ({} more lines, {} total)",
                total_lines - end,
                total_lines
            ));
        }

        AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
    }
}

fn resolve_path(file_path: &str, working_dir: &std::path::Path) -> std::path::PathBuf {
    let path = std::path::Path::new(file_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

fn is_binary_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "ico"
            | "webp"
            | "svg"
            | "mp3"
            | "mp4"
            | "avi"
            | "mov"
            | "wav"
            | "flac"
            | "zip"
            | "tar"
            | "gz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "o"
            | "a"
            | "wasm"
            | "class"
            | "pyc"
            | "pyo"
            | "db"
            | "sqlite"
            | "sqlite3"
    )
}
