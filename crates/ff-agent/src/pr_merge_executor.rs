//! Auto-merge executor for fleet-authored PRs.
//!
//! The fleet opens PRs from work_items on branches named `wi/<id>`. The pure
//! decision core [`crate::pr_integration::pr_merge_decision`] decides whether a
//! given PR is safe to land, but nothing executed it — so green fleet PRs piled
//! up unmerged (~20 in queue). This module runs that decision against every open
//! `wi/` PR and merges the ones it clears.
//!
//! Intended to run as a **leader-gated forgefleetd tick every ~2 min** (wiring
//! into the daemon is a follow-up). Every `gh` invocation is guarded so a CLI
//! failure logs-and-continues rather than aborting the pass.

use anyhow::Result;
use serde::Deserialize;
use std::process::Command;
use tracing::{info, warn};

use crate::pr_integration::{MergeDecision, pr_merge_decision};

/// Result of one auto-merge pass.
#[derive(Debug, Default, Clone)]
pub struct PrMergeReport {
    pub considered: usize,
    pub merged: usize,
    pub held: usize,
    pub blocked: usize,
    pub details: Vec<String>,
}

/// One row of `gh pr list --json number,headRefName,mergeable,files`.
#[derive(Debug, Deserialize)]
struct GhPr {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    /// "MERGEABLE" | "CONFLICTING" | "UNKNOWN".
    mergeable: Option<String>,
    #[serde(default)]
    files: Vec<serde_json::Value>,
}

/// Aggregate CI state derived from `gh pr checks <n> --json bucket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CiCounts {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

impl CiCounts {
    /// All checks reported and every one passed.
    pub fn all_passed(&self) -> bool {
        self.total > 0 && self.failed == 0 && self.passed == self.total
    }
}

/// Parse `gh pr checks --json bucket` output (a JSON array of objects each with
/// a `bucket` field: "pass" | "fail" | "pending" | "skipping" | "cancel").
/// Pure so it can be unit-tested without invoking `gh`.
pub fn parse_ci_counts(gh_checks_json: &str) -> CiCounts {
    let rows: Vec<serde_json::Value> = serde_json::from_str(gh_checks_json).unwrap_or_default();
    let mut c = CiCounts {
        total: 0,
        passed: 0,
        failed: 0,
    };
    for r in &rows {
        let bucket = r.get("bucket").and_then(|b| b.as_str()).unwrap_or("");
        // "skipping"/"cancel" don't count toward total (neither pass nor fail
        // required); everything else is a required check.
        match bucket {
            "pass" => {
                c.total += 1;
                c.passed += 1;
            }
            "fail" => {
                c.total += 1;
                c.failed += 1;
            }
            "pending" => {
                c.total += 1;
            }
            _ => {}
        }
    }
    c
}

/// Run one auto-merge pass over open `wi/` PRs. Merges (squash + delete-branch)
/// every PR the decision core clears; leaves the rest with a recorded reason.
/// `pool` is accepted for signature symmetry with other leader-gated passes /
/// future audit logging; the current implementation drives entirely off `gh`.
pub async fn run_pr_merge_pass(_pool: &sqlx::PgPool) -> Result<PrMergeReport> {
    let mut report = PrMergeReport::default();

    let list_out = Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,headRefName,mergeable,files",
            "--limit",
            "50",
        ])
        .output();
    let list_out = match list_out {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            warn!(
                stderr = %String::from_utf8_lossy(&o.stderr),
                "pr_merge_executor: gh pr list failed"
            );
            return Ok(report);
        }
        Err(e) => {
            warn!(error = %e, "pr_merge_executor: gh not runnable");
            return Ok(report);
        }
    };

    let prs: Vec<GhPr> = serde_json::from_slice(&list_out).unwrap_or_default();
    for pr in prs
        .into_iter()
        .filter(|p| p.head_ref_name.starts_with("wi/"))
    {
        report.considered += 1;
        let n = pr.number;
        let has_conflicts = pr.mergeable.as_deref() == Some("CONFLICTING");
        let files_changed = pr.files.len();

        // CI status for this PR.
        let ci = match Command::new("gh")
            .args(["pr", "checks", &n.to_string(), "--json", "bucket"])
            .output()
        {
            Ok(o) => parse_ci_counts(&String::from_utf8_lossy(&o.stdout)),
            Err(e) => {
                warn!(pr = n, error = %e, "pr_merge_executor: gh pr checks failed");
                report.held += 1;
                report.details.push(format!("#{n}: checks unavailable"));
                continue;
            }
        };
        let ci_passed = ci.all_passed();
        // Until a dedicated verify-gate signal exists, treat verify-green as
        // "all CI checks (which include fmt/clippy/check) passed".
        let is_verify_green = ci_passed;

        let decision = pr_merge_decision(
            ci_passed,
            ci.total,
            ci.passed,
            has_conflicts,
            files_changed,
            is_verify_green,
        );

        match decision {
            MergeDecision::AutoMerge => {
                match Command::new("gh")
                    .args(["pr", "merge", &n.to_string(), "--squash", "--delete-branch"])
                    .output()
                {
                    Ok(o) if o.status.success() => {
                        report.merged += 1;
                        report.details.push(format!("#{n}: auto-merged"));
                        info!(pr = n, "pr_merge_executor: auto-merged fleet PR");
                    }
                    Ok(o) => {
                        report.held += 1;
                        let err = String::from_utf8_lossy(&o.stderr);
                        report
                            .details
                            .push(format!("#{n}: merge failed ({})", err.trim()));
                        warn!(pr = n, stderr = %err, "pr_merge_executor: gh pr merge failed");
                    }
                    Err(e) => {
                        report.held += 1;
                        report.details.push(format!("#{n}: merge errored"));
                        warn!(pr = n, error = %e, "pr_merge_executor: gh pr merge errored");
                    }
                }
            }
            MergeDecision::HoldForReview(reason) => {
                report.held += 1;
                report.details.push(format!("#{n}: hold — {reason}"));
            }
            MergeDecision::Block(reason) => {
                report.blocked += 1;
                report.details.push(format!("#{n}: block — {reason}"));
            }
        }
    }

    info!(
        considered = report.considered,
        merged = report.merged,
        held = report.held,
        blocked = report.blocked,
        "pr_merge_executor: auto-merge pass complete"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ci_counts_all_pass() {
        let json = r#"[{"bucket":"pass"},{"bucket":"pass"},{"bucket":"pass"}]"#;
        let c = parse_ci_counts(json);
        assert_eq!(c.total, 3);
        assert_eq!(c.passed, 3);
        assert_eq!(c.failed, 0);
        assert!(c.all_passed());
    }

    #[test]
    fn parse_ci_counts_pending_and_fail_not_green() {
        let pending = parse_ci_counts(r#"[{"bucket":"pass"},{"bucket":"pending"}]"#);
        assert_eq!(pending.total, 2);
        assert_eq!(pending.passed, 1);
        assert!(!pending.all_passed()); // a pending check → not all passed

        let failed = parse_ci_counts(r#"[{"bucket":"pass"},{"bucket":"fail"}]"#);
        assert_eq!(failed.failed, 1);
        assert!(!failed.all_passed());
    }

    #[test]
    fn parse_ci_counts_ignores_skipped_and_empty() {
        // skipping/cancel don't count; empty/garbage → zero, not-green.
        let skipped = parse_ci_counts(r#"[{"bucket":"pass"},{"bucket":"skipping"}]"#);
        assert_eq!(skipped.total, 1);
        assert!(skipped.all_passed());
        assert!(!parse_ci_counts("not json").all_passed());
        assert!(!parse_ci_counts("[]").all_passed());
    }
}
