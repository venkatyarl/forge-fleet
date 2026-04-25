//! Agentic tools — self-reflection, verification, delegation, planning.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

/// VerifyAndRetry — run a check, if it fails auto-retry with diagnosis.
pub struct VerifyAndRetryTool;

#[async_trait]
impl AgentTool for VerifyAndRetryTool {
    fn name(&self) -> &str {
        "VerifyAndRetry"
    }
    fn description(&self) -> &str {
        "Run a verification command (test, build, lint). If it fails, return the error output for diagnosis. Use this after making code changes to verify correctness."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command to run for verification (e.g. 'cargo test', 'npm test')" },
                "max_retries": { "type": "number", "description": "Max retry attempts (default: 0, just report)" },
                "description": { "type": "string", "description": "What we're verifying" }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("verification");
        if command.is_empty() {
            return AgentToolResult::err("Missing 'command'");
        }

        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.working_dir)
            .output()
            .await;

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{stdout}{stderr}");

                if out.status.success() {
                    AgentToolResult::ok(format!(
                        "PASS: {description}\n\n{}",
                        truncate_output(&combined, 2000)
                    ))
                } else {
                    AgentToolResult::err(format!(
                        "FAIL: {description}\nExit code: {}\n\n{}\n\nDiagnose the error above and fix the issue.",
                        out.status.code().unwrap_or(-1),
                        truncate_output(&combined, MAX_TOOL_RESULT_CHARS - 200)
                    ))
                }
            }
            Err(e) => AgentToolResult::err(format!("Command failed to run: {e}")),
        }
    }
}

/// Delegate — route a subtask to a specialized agent role on a fleet node.
pub struct DelegateTool;

#[async_trait]
impl AgentTool for DelegateTool {
    fn name(&self) -> &str {
        "Delegate"
    }
    fn description(&self) -> &str {
        "Delegate a subtask to a specialized agent role (e.g. security-auditor, test-writer, researcher). The task runs on a fleet node with the best model for that role. Returns the result."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The task to delegate" },
                "role": {
                    "type": "string",
                    "enum": ["rust-developer", "typescript-developer", "python-developer", "security-auditor",
                             "test-writer", "test-runner", "code-reviewer", "devops-engineer", "database-admin",
                             "documentation-writer", "researcher", "project-planner", "bug-hunter",
                             "refactoring-specialist", "performance-optimizer", "api-designer"],
                    "description": "Agent role to delegate to"
                },
                "context": { "type": "string", "description": "Additional context for the delegate" }
            },
            "required": ["task", "role"]
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let task = input.get("task").and_then(Value::as_str).unwrap_or("");
        let role = input
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("researcher");
        let context = input.get("context").and_then(Value::as_str).unwrap_or("");

        if task.is_empty() {
            return AgentToolResult::err("Missing 'task'");
        }

        // Look up role and get system prompt extension
        let role_def = crate::agent_roles::find_role(role);
        let role_prompt = role_def
            .as_ref()
            .map(|r| r.system_prompt_extension.as_str())
            .unwrap_or("");

        let full_prompt = if context.is_empty() {
            format!("{role_prompt}\n\nTask: {task}")
        } else {
            format!("{role_prompt}\n\nContext: {context}\n\nTask: {task}")
        };

        // Use the Agent tool to spawn a sub-agent
        let agent_input = json!({
            "prompt": full_prompt,
            "description": format!("delegate to {role}"),
            "max_turns": 10
        });

        let agent_tool = super::agent_tool::SubAgentTool;
        agent_tool.execute(agent_input, ctx).await
    }
}

/// PdfExtract — extract text from PDF files.
pub struct PdfExtractTool;

#[async_trait]
impl AgentTool for PdfExtractTool {
    fn name(&self) -> &str {
        "PdfExtract"
    }
    fn description(&self) -> &str {
        "Extract text content from PDF files. Uses pdftotext if available, falls back to basic extraction."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to the PDF file" },
                "pages": { "type": "string", "description": "Page range (e.g. '1-5', default: all)" }
            },
            "required": ["file_path"]
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        if file_path.is_empty() {
            return AgentToolResult::err("Missing 'file_path'");
        }

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        if !path.exists() {
            return AgentToolResult::err(format!("File not found: {}", path.display()));
        }

        // Try pdftotext first
        let mut cmd = Command::new("pdftotext");
        cmd.arg(&path).arg("-"); // output to stdout
        if let Some(pages) = input.get("pages").and_then(Value::as_str) {
            if let Some((start, end)) = pages.split_once('-') {
                cmd.arg("-f").arg(start).arg("-l").arg(end);
            }
        }

        match cmd.output().await {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                AgentToolResult::ok(truncate_output(&text, MAX_TOOL_RESULT_CHARS))
            }
            _ => {
                // Fallback: try python pdfplumber
                let py_cmd = format!(
                    "python3 -c \"import pdfplumber; pdf=pdfplumber.open('{}'); print('\\n'.join(p.extract_text() or '' for p in pdf.pages[:20]))\"",
                    path.display()
                );
                match Command::new("bash").arg("-c").arg(&py_cmd).output().await {
                    Ok(out) if out.status.success() => {
                        AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&out.stdout), MAX_TOOL_RESULT_CHARS))
                    }
                    _ => AgentToolResult::err("PDF extraction failed. Install pdftotext (poppler-utils) or python pdfplumber.".to_string()),
                }
            }
        }
    }
}

/// SpreadsheetQuery — read and query CSV/Excel files.
pub struct SpreadsheetQueryTool;

#[async_trait]
impl AgentTool for SpreadsheetQueryTool {
    fn name(&self) -> &str {
        "SpreadsheetQuery"
    }
    fn description(&self) -> &str {
        "Read and query CSV or Excel files. Extract data, filter rows, get statistics."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to CSV or Excel file" },
                "action": { "type": "string", "enum": ["read", "head", "stats", "query"], "description": "Action (default: head)" },
                "query": { "type": "string", "description": "For query action: awk/csvq expression" },
                "rows": { "type": "number", "description": "Number of rows for head (default: 20)" }
            },
            "required": ["file_path"]
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("head");
        let rows = input.get("rows").and_then(Value::as_u64).unwrap_or(20);

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        if !path.exists() {
            return AgentToolResult::err(format!("File not found: {}", path.display()));
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match (action, ext.as_str()) {
            ("head", "csv") | ("read", "csv") => {
                match Command::new("head")
                    .arg("-n")
                    .arg(rows.to_string())
                    .arg(&path)
                    .output()
                    .await
                {
                    Ok(out) => AgentToolResult::ok(truncate_output(
                        &String::from_utf8_lossy(&out.stdout),
                        MAX_TOOL_RESULT_CHARS,
                    )),
                    Err(e) => AgentToolResult::err(format!("Failed: {e}")),
                }
            }
            ("stats", "csv") => {
                let cmd = format!(
                    "wc -l '{}' && head -1 '{}' | tr ',' '\\n' | wc -l",
                    path.display(),
                    path.display()
                );
                match Command::new("bash")
                    .arg("-c")
                    .arg(&cmd)
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                {
                    Ok(out) => AgentToolResult::ok(format!(
                        "CSV stats:\n{}",
                        String::from_utf8_lossy(&out.stdout)
                    )),
                    Err(e) => AgentToolResult::err(format!("Stats failed: {e}")),
                }
            }
            (_, "xlsx" | "xls") => {
                // Try python for Excel
                let py_cmd = format!(
                    "python3 -c \"import openpyxl; wb=openpyxl.load_workbook('{}'); ws=wb.active; [print(','.join(str(c.value or '') for c in row)) for row in list(ws.iter_rows())[:{}]]\"",
                    path.display(),
                    rows
                );
                match Command::new("bash").arg("-c").arg(&py_cmd).output().await {
                    Ok(out) if out.status.success() => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&out.stdout), MAX_TOOL_RESULT_CHARS)),
                    _ => AgentToolResult::err("Excel reading requires python openpyxl. Install with: pip install openpyxl".to_string()),
                }
            }
            _ => AgentToolResult::err(format!(
                "Unsupported file type: {ext}. Supports: csv, xlsx, xls"
            )),
        }
    }
}
