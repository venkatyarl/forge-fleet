//! FileEdit tool — exact string replacement in files.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct FileEditTool;

#[async_trait]
impl AgentTool for FileEditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Make exact string replacements in a file. Specify the old text and new text. The old_string must match exactly (including whitespace and indentation). If old_string appears multiple times, use replace_all or provide more context to make it unique."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences (default false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => return AgentToolResult::err("Missing or empty 'file_path' parameter"),
        };

        let old_string = match input.get("old_string").and_then(Value::as_str) {
            Some(s) => s,
            None => return AgentToolResult::err("Missing 'old_string' parameter"),
        };

        let new_string = match input.get("new_string").and_then(Value::as_str) {
            Some(s) => s,
            None => return AgentToolResult::err("Missing 'new_string' parameter"),
        };

        if old_string == new_string {
            return AgentToolResult::err("old_string and new_string are identical");
        }

        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let path = match resolve_path(file_path, &ctx.working_dir) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };

        // Hold the per-session edit lock across the read-modify-write so a
        // concurrent Edit/Write in the same turn can't lose this update.
        let _edit_guard = ctx.edit_lock.lock().await;

        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                return AgentToolResult::err(format!("Failed to read {}: {e}", path.display()));
            }
        };

        let count = content.matches(old_string).count();

        if count == 0 {
            return AgentToolResult::err(format!(
                "old_string not found in {}. Make sure the string matches exactly, including whitespace and indentation.",
                path.display()
            ));
        }

        if count > 1 && !replace_all {
            return AgentToolResult::err(format!(
                "old_string appears {count} times in {}. Provide more context to make it unique, or set replace_all to true.",
                path.display()
            ));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            // Replace only the first occurrence
            if let Some(pos) = content.find(old_string) {
                let mut result = String::with_capacity(content.len());
                result.push_str(&content[..pos]);
                result.push_str(new_string);
                result.push_str(&content[pos + old_string.len()..]);
                result
            } else {
                content.clone()
            }
        };

        match fs::write(&path, &new_content).await {
            Ok(()) => {
                let replacements = if replace_all { count } else { 1 };
                AgentToolResult::ok(format!(
                    "Successfully edited {} ({replacements} replacement{})",
                    path.display(),
                    if replacements > 1 { "s" } else { "" }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn test_ctx(dir: &std::path::Path) -> AgentToolContext {
        AgentToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "test-session".to_string(),
            shell_state: Arc::new(Mutex::new(Default::default())),
            edit_lock: Arc::new(Mutex::new(())),
            pg_pool: None,
        }
    }

    /// Concurrent Edit calls in the same session must not lose updates: each
    /// read-modify-write holds the session edit lock, so every replacement
    /// survives even when the agent loop runs the calls in parallel.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_edits_are_serialized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("target.txt");
        let n = 16usize;
        // Zero-padded tokens so no old_string is a substring of another.
        let initial: String = (0..n).map(|i| format!("old{i:02}\n")).collect();
        fs::write(&file, &initial).await.expect("seed file");

        let ctx = test_ctx(dir.path());
        let tool = Arc::new(FileEditTool);

        let mut handles = Vec::new();
        for i in 0..n {
            let tool = Arc::clone(&tool);
            let ctx = ctx.clone();
            let path = file.display().to_string();
            handles.push(tokio::spawn(async move {
                tool.execute(
                    json!({
                        "file_path": path,
                        "old_string": format!("old{i:02}"),
                        "new_string": format!("new{i:02}"),
                    }),
                    &ctx,
                )
                .await
            }));
        }

        for handle in handles {
            let result = handle.await.expect("join edit task");
            assert!(!result.is_error, "edit failed: {}", result.content);
        }

        let final_content = fs::read_to_string(&file).await.expect("read back");
        for i in 0..n {
            assert!(
                final_content.contains(&format!("new{i:02}")),
                "lost update: new{i:02} missing from {final_content:?}"
            );
        }
    }
}
