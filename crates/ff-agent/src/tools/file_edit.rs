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

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

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
