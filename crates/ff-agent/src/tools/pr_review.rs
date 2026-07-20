//! PRReview tool — check out a pull request, run cargo tests for the affected
//! crates, and produce an LLM review verdict.
//!
//! Reviewer selection follows the cheapest-capable-first rule: the local 480B
//! fleet model is tried first, and only if it is unavailable does the tool fall
//! back to a cloud CLI reviewer (codex → claude → kimi).

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::process::Command;
use tokio::sync::Semaphore;

use super::{AgentTool, AgentToolContext, AgentToolResult, truncate_output};

pub struct PrReviewTool;

#[async_trait]
impl AgentTool for PrReviewTool {
    fn name(&self) -> &str {
        "PRReview"
    }

    fn description(&self) -> &str {
        "Check out a pull request branch, run cargo tests for the affected crates, \
         and produce an LLM review verdict. Prefers the cheapest capable reviewer \
         (local 480B) and falls back to a cloud CLI if needed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pr_url": {
                    "type": "string",
                    "description": "Full GitHub pull request URL (preferred)"
                },
                "pr_number": {
                    "type": "number",
                    "description": "PR number when the working directory is the target repo"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let cwd = &ctx.working_dir;

        let arg = match resolve_pr_arg(&input) {
            Some(a) => a,
            None => {
                return AgentToolResult::err("Provide either `pr_url` or `pr_number`".to_string());
            }
        };

        let info = match resolve_pr_info(cwd, &arg).await {
            Ok(i) => i,
            Err(e) => return AgentToolResult::err(e),
        };

        let original_branch = match current_branch(cwd).await {
            Ok(b) => b,
            Err(e) => return AgentToolResult::err(e),
        };

        if let Err(e) = checkout_pr(cwd, &arg, info.number).await {
            return AgentToolResult::err(e);
        }

        // Gather diff + affected crates + test results, then restore the original branch.
        let diff = build_diff(cwd, &info.base_ref).await.unwrap_or_default();
        let affected = affected_crate_names(cwd, &info.base_ref)
            .await
            .unwrap_or_default();
        let test_results = run_tests_for_crates(cwd, &affected).await;

        // Best-effort restore; a failure here should not mask the review result.
        let _ = restore_branch(cwd, &original_branch).await;

        let prompt = build_review_prompt(&info, &affected, &test_results, &diff);

        let (verdict, reason, backend) =
            match generate_verdict(&prompt, cwd, ctx.pg_pool.as_ref()).await {
                Ok(v) => v,
                Err(e) => return AgentToolResult::err(e),
            };

        let result = json!({
            "verdict": verdict,
            "reason": reason,
            "backend": backend,
            "affected_crates": affected,
            "tests": test_results.iter().map(|t| json!({
                "crate": t.crate_name,
                "success": t.success,
                "output": truncate_output(&t.output, 2_000)
            })).collect::<Vec<_>>(),
            "diff_chars": diff.len()
        });

        AgentToolResult::ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// PR metadata
// ---------------------------------------------------------------------------

struct PrInfo {
    #[allow(dead_code)]
    number: u64,
    title: String,
    body: String,
    base_ref: String,
    head_ref: String,
}

fn resolve_pr_arg(input: &Value) -> Option<String> {
    if let Some(url) = input.get("pr_url").and_then(Value::as_str) {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(n) = input.get("pr_number").and_then(Value::as_u64) {
        return Some(n.to_string());
    }
    None
}

async fn resolve_pr_info(cwd: &Path, arg: &str) -> Result<PrInfo, String> {
    let mut cmd = gh_cmd(cwd).await;
    cmd.args([
        "pr",
        "view",
        arg,
        "--json",
        "number,title,body,baseRefName,headRefName",
    ]);
    let stdout = run_cmd(cmd, 120).await?;

    let v: Value = serde_json::from_str(&stdout).map_err(|e| format!("parse gh pr view: {e}"))?;

    Ok(PrInfo {
        number: v
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| "missing PR number".to_string())?,
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        body: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_ref: v
            .get("baseRefName")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing baseRefName".to_string())?
            .to_string(),
        head_ref: v
            .get("headRefName")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing headRefName".to_string())?
            .to_string(),
    })
}

async fn current_branch(cwd: &Path) -> Result<String, String> {
    let mut cmd = git_cmd(cwd);
    cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]);
    run_cmd(cmd, 30).await.map(|s| s.trim().to_string())
}

async fn restore_branch(cwd: &Path, branch: &str) -> Result<(), String> {
    let mut cmd = git_cmd(cwd);
    cmd.args(["checkout", branch]);
    run_cmd(cmd, 60).await?;
    Ok(())
}

async fn checkout_pr(cwd: &Path, arg: &str, number: u64) -> Result<String, String> {
    let branch = format!("ff-pr-review-{number}");
    let mut cmd = gh_cmd(cwd).await;
    cmd.args(["pr", "checkout", arg, "--branch", &branch]);
    run_cmd(cmd, 180).await?;
    Ok(branch)
}

// ---------------------------------------------------------------------------
// Affected crates
// ---------------------------------------------------------------------------

async fn affected_crate_names(cwd: &Path, base_ref: &str) -> Result<Vec<String>, String> {
    // Make sure the base ref is available locally.
    let mut fetch = git_cmd(cwd);
    fetch.args(["fetch", "origin", base_ref]);
    run_cmd(fetch, 120).await?;

    let remote_base = format!("origin/{base_ref}");
    let mut diff = git_cmd(cwd);
    diff.args(["diff", "--name-only", &format!("{remote_base}...HEAD")]);
    let stdout = run_cmd(diff, 60).await?;

    let mut seen = HashSet::new();
    let mut crates = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = crate_name_for_path(cwd, line) {
            if seen.insert(name.clone()) {
                crates.push(name);
            }
        }
    }

    Ok(crates)
}

fn crate_name_for_path(cwd: &Path, rel: &str) -> Option<String> {
    let file_path = cwd.join(rel);
    let mut dir = file_path.parent()?;

    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file() {
            if let Ok(text) = std::fs::read_to_string(&cargo) {
                if let Ok(value) = toml::from_str::<toml::Value>(&text) {
                    if let Some(name) = value
                        .get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        return Some(name.to_string());
                    }
                }
            }
        }
        if dir == cwd {
            break;
        }
        dir = dir.parent()?;
    }
    None
}

// ---------------------------------------------------------------------------
// Cargo tests
// ---------------------------------------------------------------------------

struct TestOutcome {
    crate_name: String,
    success: bool,
    output: String,
}

async fn run_tests_for_crates(cwd: &Path, crates: &[String]) -> Vec<TestOutcome> {
    if crates.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(crates.len());
    for name in crates {
        results.push(run_cargo_test(cwd, name).await);
    }
    results
}

async fn run_cargo_test(cwd: &Path, crate_name: &str) -> TestOutcome {
    let mut cmd = Command::new("cargo");
    cmd.arg("+1.88.0")
        .args(["test", "-p", crate_name, "--lib"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let timeout = Duration::from_secs(900);
    let outcome = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => {
            let status = if out.status.success() { "PASS" } else { "FAIL" };
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            TestOutcome {
                crate_name: crate_name.to_string(),
                success: out.status.success(),
                output: format!(
                    "[{status}] cargo +1.88.0 test -p {crate_name} --lib\n{stdout}{stderr}"
                ),
            }
        }
        Ok(Err(e)) => TestOutcome {
            crate_name: crate_name.to_string(),
            success: false,
            output: format!("[FAIL] cargo test -p {crate_name}: {e}"),
        },
        Err(_) => TestOutcome {
            crate_name: crate_name.to_string(),
            success: false,
            output: format!(
                "[FAIL] cargo test -p {crate_name} timed out after {}s",
                timeout.as_secs()
            ),
        },
    };

    TestOutcome {
        output: truncate_output(&outcome.output, 8_000),
        ..outcome
    }
}

// ---------------------------------------------------------------------------
// Diff + prompt
// ---------------------------------------------------------------------------

async fn build_diff(cwd: &Path, base_ref: &str) -> Result<String, String> {
    let remote_base = format!("origin/{base_ref}");
    let mut cmd = git_cmd(cwd);
    cmd.args(["diff", "--no-color", &format!("{remote_base}...HEAD")]);
    let stdout = run_cmd(cmd, 120).await?;
    Ok(truncate_chars(&stdout, 40_000))
}

fn build_review_prompt(
    info: &PrInfo,
    affected: &[String],
    tests: &[TestOutcome],
    diff: &str,
) -> String {
    let test_summary: String = if tests.is_empty() {
        "No Rust crate changes detected; no cargo tests were run.".to_string()
    } else {
        tests
            .iter()
            .map(|t| {
                let status = if t.success { "PASS" } else { "FAIL" };
                format!(
                    "- {status}: {}\n{}",
                    t.crate_name,
                    truncate_chars(&t.output, 1_500)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "You are reviewing a pull request.\n\n\
         Title: {title}\n\
         Base: {base} → Head: {head}\n\n\
         Description:\n{body}\n\n\
         Affected crates: {affected}\n\n\
         Cargo test results ({n} crates):\n{tests}\n\n\
         Judge whether the change correctly and cleanly implements the PR intent.\n\
         Requirements for approval:\n\
         - The diff matches the stated intent.\n\
         - The affected-crate tests pass (or any failures are clearly unrelated).\n\
         - The change does not degrade existing code, docs, comments, tests, or behavior.\n\
         - The change is complete, not a placeholder or partial implementation.\n\n\
         Answer with exactly APPROVE or REJECT on the first line. Put a concise reason on the next line.\n\n\
         Diff (truncated):\n```diff\n{diff}\n```",
        title = info.title,
        base = info.base_ref,
        head = info.head_ref,
        body = truncate_chars(&info.body, 4_000),
        affected = if affected.is_empty() {
            "none".to_string()
        } else {
            affected.join(", ")
        },
        n = tests.len(),
        tests = test_summary,
        diff = diff
    )
}

// ---------------------------------------------------------------------------
// Reviewer selection: local 480B first, then cloud CLI fallback
// ---------------------------------------------------------------------------

const REVIEWER_480B_HINT: &str = "480b";

static REVIEW_480B_GATE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));

async fn generate_verdict(
    prompt: &str,
    cwd: &Path,
    pg_pool: Option<&PgPool>,
) -> Result<(String, String, String), String> {
    // Try the cheapest reviewer first when we have a Postgres pool to route it.
    if let Some(pool) = pg_pool {
        match review_via_480b(pool, prompt).await {
            Ok((approved, reason)) => {
                let verdict = if approved { "APPROVE" } else { "REJECT" };
                return Ok((verdict.to_string(), reason, "480b".to_string()));
            }
            Err(e) => {
                // Fall through to cloud CLI reviewers.
                tracing::info!(error = %e, "pr_review: 480b unavailable, falling back to cloud CLI");
            }
        }
    }

    let (approved, reason, backend) = cloud_cli_review(prompt, cwd).await?;
    let verdict = if approved { "APPROVE" } else { "REJECT" };
    Ok((verdict.to_string(), reason, backend))
}

async fn review_via_480b(pool: &PgPool, prompt: &str) -> Result<(bool, String), String> {
    let _permit = REVIEW_480B_GATE
        .acquire()
        .await
        .map_err(|e| format!("480b review gate closed: {e}"))?;

    let resp = crate::fleet_oneshot::fleet_oneshot(
        pool,
        prompt,
        Some(REVIEWER_480B_HINT),
        Some(Duration::from_secs(300)),
    )
    .await
    .map_err(|e| format!("480b review failed: {e}"))?;

    if !served_by_480b(&resp.model) {
        return Err(format!(
            "480b ring unavailable — fleet_oneshot served by {} on {}",
            resp.model, resp.worker_name
        ));
    }

    Ok(parse_review_response(&resp.text))
}

async fn cloud_cli_review(prompt: &str, cwd: &Path) -> Result<(bool, String, String), String> {
    let mut last_err = String::new();
    for backend in ["codex", "claude", "kimi"] {
        match crate::cli_executor::execute_cli_in_dir(
            backend,
            prompt,
            &[],
            Some(cwd),
            Some(Duration::from_secs(600)),
        )
        .await
        {
            Ok(res) if res.exit_code == 0 && !res.stdout.trim().is_empty() => {
                let (approved, reason) = parse_review_response(&res.stdout);
                return Ok((approved, reason, backend.to_string()));
            }
            Ok(res) => {
                let msg = format!(
                    "{backend} exited {}: {}",
                    res.exit_code,
                    res.stderr.trim().chars().take(300).collect::<String>()
                );
                tracing::info!(error = %msg, "pr_review: cloud backend failed");
                last_err = msg;
            }
            Err(e) => {
                let msg = format!("{backend} unavailable: {e}");
                tracing::info!(error = %msg, "pr_review: cloud backend unavailable");
                last_err = msg;
            }
        }
    }
    Err(format!("no cloud CLI reviewer available: {last_err}"))
}

fn served_by_480b(model: &str) -> bool {
    model.to_lowercase().contains(REVIEWER_480B_HINT)
}

fn parse_review_response(response: &str) -> (bool, String) {
    let mut first_idx = None;
    let mut first_line = "";
    for (idx, line) in response.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            first_idx = Some(idx);
            first_line = trimmed;
            break;
        }
    }

    let Some(idx) = first_idx else {
        return (false, "empty review response".to_string());
    };

    let approved = first_line.to_uppercase().starts_with("APPROVE");
    let reason = response
        .lines()
        .skip(idx + 1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let reason = if reason.is_empty() {
        first_line.to_string()
    } else {
        reason
    };
    (approved, reason)
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

async fn gh_cmd(cwd: &Path) -> Command {
    let mut c = Command::new("gh");
    c.current_dir(cwd);
    if let Some(token) = crate::fleet_info::fetch_secret("github_gh_token").await {
        c.env("GH_TOKEN", token);
    }
    c
}

fn git_cmd(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd);
    c
}

async fn run_cmd(mut cmd: Command, timeout_secs: u64) -> Result<String, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let timeout = Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "command failed ({}): {}",
                out.status,
                stderr.chars().take(800).collect::<String>()
            ))
        }
        Ok(Err(e)) => Err(format!("command spawn failed: {e}")),
        Err(_) => Err(format!("command timed out after {timeout_secs}s")),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_review_response_verdicts() {
        let (approved, reason) = parse_review_response("APPROVE\nmatches intent");
        assert!(approved);
        assert_eq!(reason, "matches intent");

        let (approved, reason) = parse_review_response("\nREJECT\nplaceholder diff");
        assert!(!approved);
        assert_eq!(reason, "placeholder diff");

        let (approved, reason) = parse_review_response("");
        assert!(!approved);
        assert_eq!(reason, "empty review response");
    }

    #[test]
    fn served_by_480b_matches_the_ring_only() {
        assert!(served_by_480b("qwen3-coder-480b"));
        assert!(served_by_480b("Qwen3-Coder-480B-A35B"));
        assert!(!served_by_480b("qwen3-coder-30b"));
        assert!(!served_by_480b("local"));
        assert!(!served_by_480b(""));
    }

    #[test]
    fn crate_name_for_path_finds_nearest_package() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        std::fs::create_dir_all(root.join("crates/foo/src")).unwrap();
        let mut cargo = std::fs::File::create(root.join("crates/foo/Cargo.toml")).unwrap();
        cargo.write_all(b"[package]\nname = \"foo\"\n").unwrap();

        assert_eq!(
            crate_name_for_path(root, "crates/foo/src/lib.rs").as_deref(),
            Some("foo")
        );
        assert_eq!(
            crate_name_for_path(root, "crates/foo/Cargo.toml").as_deref(),
            Some("foo")
        );
        assert!(crate_name_for_path(root, "README.md").is_none());
    }

    #[test]
    fn resolve_pr_arg_prefers_url() {
        let input = json!({"pr_url": "https://github.com/o/r/pull/42", "pr_number": 7});
        assert_eq!(
            resolve_pr_arg(&input).as_deref(),
            Some("https://github.com/o/r/pull/42")
        );
    }

    #[test]
    fn resolve_pr_arg_falls_back_to_number() {
        let input = json!({"pr_number": 42});
        assert_eq!(resolve_pr_arg(&input).as_deref(), Some("42"));
    }

    #[test]
    fn resolve_pr_arg_requires_input() {
        assert!(resolve_pr_arg(&json!({})).is_none());
    }
}
