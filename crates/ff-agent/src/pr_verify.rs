//! Verify gate for fleet-authored PRs (autonomy roadmap #1).
//!
//! The fleet self-builds: codex writes code in a sub-agent worktree, the
//! salvage step commits the diff and opens a PR — even when codex fails to exit
//! cleanly. Nothing currently checks that the diff actually compiles before it
//! becomes a mergeable PR, so a bad change (e.g. fleet PR #679 used
//! `WHERE name = $2` when the column is `worker_name`) can slip through.
//!
//! This module runs the standard Rust checks against a worktree and returns a
//! structured [`VerifyReport`]. The dispatch / merge flow can then block a PR
//! (or requeue the work item) when `passed()` is false. Wiring into the merge
//! flow is a follow-up; this is the reusable gate itself.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Wall-clock cap for each individual check. A cold `cargo check` against the
/// full workspace is minutes; clippy is longer. Kept generous so a slow-but-
/// progressing build isn't falsely failed, but bounded so a wedged check can't
/// hang the gate forever.
const CHECK_TIMEOUT: Duration = Duration::from_secs(900);

/// How many lines of a failing check's output to retain in `details`.
const DETAIL_LINES: usize = 40;

/// Result of running the Rust verify checks against a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// `cargo fmt --check` passed (no formatting diffs).
    pub fmt_ok: bool,
    /// `cargo check` passed (compiles).
    pub check_ok: bool,
    /// `cargo clippy -- -D warnings` passed (no lint errors).
    pub clippy_ok: bool,
    /// True iff all three checks passed.
    pub passed: bool,
    /// Human-readable summary + captured error lines from any failing check.
    pub details: String,
}

impl VerifyReport {
    /// Whether every gated check passed. Mirrors the `passed` field so callers
    /// can use either the method or the field interchangeably.
    pub fn passed(&self) -> bool {
        self.fmt_ok && self.check_ok && self.clippy_ok
    }
}

/// Outcome of one check invocation.
struct CheckOutcome {
    ok: bool,
    /// Trimmed tail of stdout+stderr when the check failed (empty on success).
    output: String,
}

/// Run one `cargo +1.88.0 <args...>` invocation in `dir`, bounded by
/// [`CHECK_TIMEOUT`]. A spawn failure or timeout counts as a failed check (the
/// gate fails closed — an un-runnable check is not a pass).
fn run_cargo(dir: &Path, args: &[&str]) -> CheckOutcome {
    let mut cmd = Command::new("cargo");
    cmd.arg("+1.88.0")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null());

    let child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CheckOutcome {
                ok: false,
                output: format!("failed to spawn `cargo {}`: {e}", args.join(" ")),
            };
        }
    };

    let out = match wait_with_timeout(child, CHECK_TIMEOUT) {
        Some(o) => o,
        None => {
            return CheckOutcome {
                ok: false,
                output: format!(
                    "`cargo {}` timed out after {}s",
                    args.join(" "),
                    CHECK_TIMEOUT.as_secs()
                ),
            };
        }
    };

    if out.status.success() {
        CheckOutcome {
            ok: true,
            output: String::new(),
        }
    } else {
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&out.stdout));
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        CheckOutcome {
            ok: false,
            output: last_lines(&combined, DETAIL_LINES),
        }
    }
}

/// Poll a child process to completion, killing it if it runs past `timeout`.
/// Returns `None` on timeout (after killing), `Some(output)` otherwise.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                // Process finished; collect its buffered output.
                return child.wait_with_output().ok();
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(_) => return None,
        }
    }
}

/// Keep the last `n` non-empty-trimmed lines of `text` (the tail is where the
/// error summary lives for cargo output).
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Run the standard Rust verify checks (`fmt --check`, `check`, `clippy -D
/// warnings`) against `worktree_path` and return a structured report.
///
/// Each check is bounded by [`CHECK_TIMEOUT`] and fails closed. This is a
/// blocking operation wrapped in `spawn_blocking` so it can be awaited from
/// async dispatch code without stalling the runtime.
pub async fn verify_worktree(worktree_path: &Path) -> VerifyReport {
    let dir = worktree_path.to_path_buf();
    tokio::task::spawn_blocking(move || verify_worktree_blocking(&dir))
        .await
        .unwrap_or_else(|e| VerifyReport {
            fmt_ok: false,
            check_ok: false,
            clippy_ok: false,
            passed: false,
            details: format!("verify task panicked/join error: {e}"),
        })
}

/// Synchronous core of [`verify_worktree`]. Runs the three checks in order and
/// assembles the report. Exposed for callers already on a blocking thread.
pub fn verify_worktree_blocking(worktree_path: &Path) -> VerifyReport {
    let fmt = run_cargo(worktree_path, &["fmt", "--check"]);
    let check = run_cargo(worktree_path, &["check"]);
    let clippy = run_cargo(worktree_path, &["clippy", "--", "-D", "warnings"]);

    let passed = fmt.ok && check.ok && clippy.ok;

    let mut details = String::new();
    details.push_str(&format!(
        "fmt: {} | check: {} | clippy: {}\n",
        pass_str(fmt.ok),
        pass_str(check.ok),
        pass_str(clippy.ok)
    ));
    for (name, outcome) in [("fmt", &fmt), ("check", &check), ("clippy", &clippy)] {
        if !outcome.ok && !outcome.output.is_empty() {
            details.push_str(&format!("\n─── {name} output ───\n{}\n", outcome.output));
        }
    }

    VerifyReport {
        fmt_ok: fmt.ok,
        check_ok: check.ok,
        clippy_ok: clippy.ok,
        passed,
        details,
    }
}

fn pass_str(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(fmt: bool, check: bool, clippy: bool) -> VerifyReport {
        VerifyReport {
            fmt_ok: fmt,
            check_ok: check,
            clippy_ok: clippy,
            passed: fmt && check && clippy,
            details: String::new(),
        }
    }

    #[test]
    fn passed_requires_all_three() {
        assert!(report(true, true, true).passed());
        assert!(!report(false, true, true).passed());
        assert!(!report(true, false, true).passed());
        assert!(!report(true, true, false).passed());
        assert!(!report(false, false, false).passed());
    }

    #[test]
    fn passed_method_matches_field() {
        for (a, b, c) in [
            (true, true, true),
            (true, false, true),
            (false, false, false),
        ] {
            let r = report(a, b, c);
            assert_eq!(r.passed(), r.passed);
        }
    }

    #[test]
    fn last_lines_keeps_tail() {
        let text = "l1\nl2\nl3\nl4\nl5";
        assert_eq!(last_lines(text, 2), "l4\nl5");
        assert_eq!(last_lines(text, 10), text);
        assert_eq!(last_lines("", 3), "");
    }
}
