//! PR integration branch builder — the git-plumbing counterpart to the pure
//! decision core in [`crate::pr_integration`].
//!
//! The fleet opens one PR per work_item (`wi/<id>`). When several of those
//! belong together (an epic decomposed into children, or a cluster that
//! references each other's symbols), landing them one-by-one races: a fragment
//! whose sibling isn't merged yet fails CI, and interleaved merges thrash the
//! base. This module takes an [`IntegrationPlan`] (the sibling branches) and
//! builds ONE integration branch off the target base by merging each child in
//! turn, so the cluster can be verified + landed as a single unit.
//!
//! Design contract:
//! - **Non-destructive.** Work happens on a scratch branch cut fresh from the
//!   base. A conflicting merge is `--abort`ed (conflict recorded, tree left
//!   clean) so a partial merge never wedges the working repo.
//! - **Reports, doesn't force.** Conflicts are surfaced per-branch with the
//!   conflicting file paths; resolution is a human's call (the merge-drain tick
//!   routes those to review). We never auto-resolve.
//! - **Pure/impure split.** The outcome types + their queries are pure and
//!   unit-tested; only [`build_integration_branch`] touches git.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

use crate::pr_integration::IntegrationPlan;

/// Result of attempting to merge one child branch into the integration branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    /// Merged cleanly.
    Merged,
    /// Merge hit conflicts; carries the conflicting file paths. The merge was
    /// aborted, so the integration branch is unchanged by this child.
    Conflicted(Vec<String>),
    /// The child branch ref could not be found on the remote.
    Missing,
}

/// Outcome of building an integration branch from a plan.
#[derive(Debug, Clone)]
pub struct IntegrationOutcome {
    /// Name of the scratch integration branch that was created.
    pub integration_branch: String,
    /// Base branch it was cut from.
    pub base: String,
    /// One entry per child branch, in plan order.
    pub results: Vec<(String, ChildOutcome)>,
}

impl IntegrationOutcome {
    /// Every child merged cleanly — the integration branch is ready to verify.
    pub fn is_clean(&self) -> bool {
        !self.results.is_empty()
            && self
                .results
                .iter()
                .all(|(_, o)| matches!(o, ChildOutcome::Merged))
    }

    /// Branches that could not be integrated (conflict or missing).
    pub fn blocked_branches(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|(_, o)| !matches!(o, ChildOutcome::Merged))
            .map(|(b, _)| b.as_str())
            .collect()
    }

    /// Human-readable one-line summary for logs / PR bodies.
    pub fn summary(&self) -> String {
        let merged = self
            .results
            .iter()
            .filter(|(_, o)| matches!(o, ChildOutcome::Merged))
            .count();
        format!(
            "integration/{} ← {}: {}/{} merged clean{}",
            self.base,
            self.base,
            merged,
            self.results.len(),
            if self.is_clean() {
                String::new()
            } else {
                format!(" · blocked: {}", self.blocked_branches().join(", "))
            }
        )
    }
}

/// Derive the scratch integration-branch name for a plan. Kept pure + stable so
/// re-running a plan targets the same branch (idempotent `checkout -B`).
pub fn integration_branch_name(plan: &IntegrationPlan) -> String {
    // `integration/<base>` with any slashes in the base flattened so the ref is
    // a single path segment (avoids `integration/feature/x` nesting surprises).
    format!("integration/{}", plan.target_branch.replace('/', "-"))
}

/// Build an integration branch by merging every child of `plan` onto a fresh
/// branch cut from `origin/<target_branch>`. Returns per-child outcomes; a
/// conflicting or missing child does NOT abort the whole run — the others still
/// merge, and the caller decides what to do with a partially-blocked result.
///
/// `repo` is the working tree to operate in. The caller is responsible for the
/// repo being on a disposable state (this switches branches). Intended to run in
/// a dedicated worktree, mirroring how sub-agent builds are isolated.
pub async fn build_integration_branch(
    repo: &Path,
    plan: &IntegrationPlan,
) -> Result<IntegrationOutcome> {
    let integ = integration_branch_name(plan);

    // Refresh remote refs so origin/<target> and the child branches are current.
    git(repo, &["fetch", "origin", "--prune", "--quiet"])
        .await
        .context("fetch origin")?;

    // Cut/reset the integration branch from the base. `-B` makes this idempotent:
    // re-running discards any prior integration attempt and starts clean.
    let base_ref = format!("origin/{}", plan.target_branch);
    git(repo, &["checkout", "-B", &integ, &base_ref])
        .await
        .with_context(|| format!("checkout -B {integ} {base_ref}"))?;

    let mut results = Vec::with_capacity(plan.child_branches.len());
    for br in &plan.child_branches {
        let outcome = merge_child(repo, br).await?;
        results.push((br.clone(), outcome));
    }

    Ok(IntegrationOutcome {
        integration_branch: integ,
        base: plan.target_branch.clone(),
        results,
    })
}

/// Merge one child branch into the current branch. Non-fast-forward so the
/// integration history records each child as a distinct merge. On conflict,
/// collects the unmerged paths and aborts, leaving the tree clean.
async fn merge_child(repo: &Path, branch: &str) -> Result<ChildOutcome> {
    // Resolve the branch to a commit; prefer the remote ref (that's what the PR
    // points at). If neither the remote nor local ref exists, it's Missing.
    let remote_ref = format!("origin/{branch}");
    let have_ref = git_ok(repo, &["rev-parse", "--verify", "--quiet", &remote_ref]).await
        || git_ok(repo, &["rev-parse", "--verify", "--quiet", branch]).await;
    if !have_ref {
        return Ok(ChildOutcome::Missing);
    }
    let merge_target = if git_ok(repo, &["rev-parse", "--verify", "--quiet", &remote_ref]).await {
        remote_ref
    } else {
        branch.to_string()
    };

    let (ok, _out) = git_capture(
        repo,
        &[
            "merge",
            "--no-ff",
            "--no-edit",
            "-m",
            &format!("integrate: {branch}"),
            &merge_target,
        ],
    )
    .await?;
    if ok {
        return Ok(ChildOutcome::Merged);
    }

    // Conflicted — capture the unmerged paths, then abort so the branch is left
    // exactly at its pre-merge state (the other children can still merge).
    let (_, conflicted) = git_capture(repo, &["diff", "--name-only", "--diff-filter=U"]).await?;
    let files: Vec<String> = conflicted
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // Best-effort abort; ignore its exit (a non-conflict merge failure has
    // nothing to abort, and we've already captured the reason).
    let _ = git_capture(repo, &["merge", "--abort"]).await;
    Ok(ChildOutcome::Conflicted(files))
}

/// Run a git command, erroring (with stderr) if it exits non-zero.
async fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let (ok, out) = git_capture(repo, args).await?;
    if ok {
        Ok(())
    } else {
        Err(anyhow!("git {}: {}", args.join(" "), out.trim()))
    }
}

/// Run a git command; return whether it succeeded (used for ref existence
/// probes where a non-zero exit is an expected "no" rather than an error).
async fn git_ok(repo: &Path, args: &[&str]) -> bool {
    git_capture(repo, args)
        .await
        .map(|(ok, _)| ok)
        .unwrap_or(false)
}

/// Run a git command in `repo`, returning (success, combined stdout+stderr).
async fn git_capture(repo: &Path, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), combined))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(base: &str, children: &[&str]) -> IntegrationPlan {
        IntegrationPlan {
            child_branches: children.iter().map(|s| s.to_string()).collect(),
            pr_numbers: vec![],
            target_branch: base.to_string(),
        }
    }

    #[test]
    fn branch_name_flattens_base_slashes() {
        assert_eq!(
            integration_branch_name(&plan("main", &[])),
            "integration/main"
        );
        assert_eq!(
            integration_branch_name(&plan("feature/x", &[])),
            "integration/feature-x"
        );
    }

    #[test]
    fn is_clean_requires_all_merged_and_nonempty() {
        let empty = IntegrationOutcome {
            integration_branch: "integration/main".into(),
            base: "main".into(),
            results: vec![],
        };
        assert!(!empty.is_clean(), "empty plan is not 'clean'");

        let all_merged = IntegrationOutcome {
            integration_branch: "integration/main".into(),
            base: "main".into(),
            results: vec![
                ("wi/a".into(), ChildOutcome::Merged),
                ("wi/b".into(), ChildOutcome::Merged),
            ],
        };
        assert!(all_merged.is_clean());
        assert!(all_merged.blocked_branches().is_empty());
    }

    #[test]
    fn blocked_branches_lists_conflicts_and_missing() {
        let o = IntegrationOutcome {
            integration_branch: "integration/main".into(),
            base: "main".into(),
            results: vec![
                ("wi/a".into(), ChildOutcome::Merged),
                (
                    "wi/b".into(),
                    ChildOutcome::Conflicted(vec!["src/main.rs".into()]),
                ),
                ("wi/c".into(), ChildOutcome::Missing),
            ],
        };
        assert!(!o.is_clean());
        assert_eq!(o.blocked_branches(), vec!["wi/b", "wi/c"]);
        assert!(o.summary().contains("blocked: wi/b, wi/c"));
        assert!(o.summary().contains("1/3 merged"));
    }
}
