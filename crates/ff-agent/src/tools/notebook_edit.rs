//! NotebookEdit tool — edit Jupyter notebook cells.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct NotebookEditTool;

#[async_trait]
impl AgentTool for NotebookEditTool {
    fn name(&self) -> &str { "NotebookEdit" }

    fn description(&self) -> &str {
        "Edit Jupyter notebook (.ipynb) cells. Can insert, replace, or delete cells by index."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the .ipynb notebook file"
                },
                "action": {
                    "type": "string",
                    "enum": ["insert", "replace", "delete"],
                    "description": "Action to perform on the cell"
                },
                "cell_index": {
                    "type": "number",
                    "description": "Index of the cell to modify (0-based)"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Type of cell (for insert/replace)"
                },
                "source": {
                    "type": "string",
                    "description": "Cell content (for insert/replace)"
                }
            },
            "required": ["file_path", "action", "cell_index"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) => p,
            None => return AgentToolResult::err("Missing 'file_path'"),
        };

        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let cell_index = input.get("cell_index").and_then(Value::as_u64).unwrap_or(0) as usize;

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        // Read notebook
        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return AgentToolResult::err(format!("Failed to read {}: {e}", path.display())),
        };

        let mut notebook: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => return AgentToolResult::err(format!("Invalid notebook JSON: {e}")),
        };

        let cells = match notebook.get_mut("cells").and_then(Value::as_array_mut) {
            Some(c) => c,
            None => return AgentToolResult::err("Notebook has no 'cells' array"),
        };

        match action {
            "delete" => {
                if cell_index >= cells.len() {
                    return AgentToolResult::err(format!("Cell index {cell_index} out of range (notebook has {} cells)", cells.len()));
                }
                cells.remove(cell_index);
            }
            "replace" | "insert" => {
                let cell_type = input.get("cell_type").and_then(Value::as_str).unwrap_or("code");
                let source = input.get("source").and_then(Value::as_str).unwrap_or("");

                let source_lines: Vec<Value> = source
                    .lines()
                    .map(|l| Value::String(format!("{l}\n")))
                    .collect();

                let outputs = if cell_type == "code" { json!([]) } else { Value::Null };
                let new_cell = json!({
                    "cell_type": cell_type,
                    "source": source_lines,
                    "metadata": {},
                    "outputs": outputs,
                    "execution_count": null
                });

                if action == "replace" {
                    if cell_index >= cells.len() {
                        return AgentToolResult::err(format!("Cell index {cell_index} out of range"));
                    }
                    cells[cell_index] = new_cell;
                } else {
                    let idx = cell_index.min(cells.len());
                    cells.insert(idx, new_cell);
                }
            }
            _ => return AgentToolResult::err(format!("Unknown action: {action}")),
        }

        // Drop mutable borrow before serializing
        let cell_count = notebook
            .get("cells")
            .and_then(Value::as_array)
            .map(|c| c.len())
            .unwrap_or(0);

        // Write back
        let output = serde_json::to_string_pretty(&notebook).unwrap_or_default();
        match fs::write(&path, &output).await {
            Ok(()) => AgentToolResult::ok(format!(
                "Notebook {}: {} cell at index {cell_index} ({cell_count} cells total)",
                path.display(),
                action,
            )),
            Err(e) => AgentToolResult::err(format!("Failed to write notebook: {e}")),
        }
    }
}
