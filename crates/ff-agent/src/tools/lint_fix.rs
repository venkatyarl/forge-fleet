//! LintFix tool — run linter/tests and auto-fix loop.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct LintFixTool;

#[async_trait]
impl AgentTool for LintFixTool {
    fn name(&self) -> &str { "LintFix" }

    fn description(&self) -> &str {
        "Run linters, formatters, and test suites. Auto-detects the project type (Rust/Node/Python) and runs the appropriate tools. Reports pass/fail with error details."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["lint", "format", "test", "check", "all"],
                    "description": "What to run (default: check)"
                },
                "fix": {
                    "type": "boolean",
                    "description": "Auto-fix issues where possible (default: false)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("check");
        let auto_fix = input.get("fix").and_then(Value::as_bool).unwrap_or(false);

        // Detect project type
        let project = detect_project_type(&ctx.working_dir).await;
        let mut results = Vec::new();

        match action {
            "lint" | "all" | "check" => {
                let lint_result = run_lint(&ctx.working_dir, &project, auto_fix).await;
                results.push(format!("Lint: {lint_result}"));
            }
            _ => {}
        }

        match action {
            "format" | "all" => {
                let fmt_result = run_format(&ctx.working_dir, &project, auto_fix).await;
                results.push(format!("Format: {fmt_result}"));
            }
            _ => {}
        }

        match action {
            "test" | "all" | "check" => {
                let test_result = run_tests(&ctx.working_dir, &project).await;
                results.push(format!("Tests: {test_result}"));
            }
            _ => {}
        }

        let output = results.join("\n\n");
        let has_errors = output.contains("FAIL") || output.contains("error");

        if has_errors {
            AgentToolResult::err(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
        } else {
            AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
        }
    }
}

enum ProjectType { Rust, Node, Python, Go, Unknown }

async fn detect_project_type(dir: &std::path::Path) -> ProjectType {
    if dir.join("Cargo.toml").exists() { ProjectType::Rust }
    else if dir.join("package.json").exists() { ProjectType::Node }
    else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() { ProjectType::Python }
    else if dir.join("go.mod").exists() { ProjectType::Go }
    else { ProjectType::Unknown }
}

async fn run_lint(dir: &std::path::Path, project: &ProjectType, fix: bool) -> String {
    let (cmd, args) = match project {
        ProjectType::Rust => ("cargo", if fix { vec!["clippy", "--fix", "--allow-dirty"] } else { vec!["clippy"] }),
        ProjectType::Node => ("npx", if fix { vec!["eslint", ".", "--fix"] } else { vec!["eslint", "."] }),
        ProjectType::Python => ("python3", vec!["-m", "ruff", "check", if fix { "--fix" } else { "" }]),
        ProjectType::Go => ("golangci-lint", vec!["run"]),
        ProjectType::Unknown => return "Unknown project type — cannot lint".into(),
    };

    run_command(dir, cmd, &args).await
}

async fn run_format(dir: &std::path::Path, project: &ProjectType, fix: bool) -> String {
    let (cmd, args) = match project {
        ProjectType::Rust => ("cargo", vec!["fmt", if fix { "" } else { "--check" }]),
        ProjectType::Node => ("npx", vec!["prettier", if fix { "--write" } else { "--check" }, "."]),
        ProjectType::Python => ("python3", vec!["-m", "ruff", "format", if fix { "" } else { "--check" }]),
        ProjectType::Go => ("gofmt", vec![if fix { "-w" } else { "-l" }, "."]),
        ProjectType::Unknown => return "Unknown project type — cannot format".into(),
    };

    run_command(dir, cmd, &args).await
}

async fn run_tests(dir: &std::path::Path, project: &ProjectType) -> String {
    let (cmd, args) = match project {
        ProjectType::Rust => ("cargo", vec!["test"]),
        ProjectType::Node => ("npm", vec!["test"]),
        ProjectType::Python => ("python3", vec!["-m", "pytest"]),
        ProjectType::Go => ("go", vec!["test", "./..."]),
        ProjectType::Unknown => return "Unknown project type — cannot test".into(),
    };

    run_command(dir, cmd, &args).await
}

async fn run_command(dir: &std::path::Path, cmd: &str, args: &[&str]) -> String {
    let args: Vec<&str> = args.iter().filter(|a| !a.is_empty()).copied().collect();
    match Command::new(cmd).args(&args).current_dir(dir).output().await {
        Ok(out) => {
            let status = if out.status.success() { "PASS" } else { "FAIL" };
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!("[{status}] {cmd} {}\n{stdout}{stderr}", args.join(" "))
        }
        Err(e) => format!("[FAIL] {cmd}: {e}"),
    }
}
