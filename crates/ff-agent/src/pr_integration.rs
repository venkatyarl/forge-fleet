//! PR integration policy — the pure decision core for the fleet's auto-merge bot
//! (council roadmap #7).
//!
//! The fleet opens a PR per work_item (branch `wi/<id>`); the merge-drain tick
//! ([`crate::work_item_merge_drain`]) walks the queue. Before a fleet-authored
//! PR can land without a human traffic-controller, something has to decide
//! *which* PRs are safe to auto-merge, which to hold for review, and which to
//! block. That judgment is factored out here as a single pure function so it is
//! trivially unit-testable and independent of the GitHub/DB plumbing — the
//! merge-drain tick will gather the inputs (CI results, conflict status, diff
//! size, verify-gate result from [`crate::pr_verify`]) and consume this
//! decision next.

/// What the merge-drain tick should do with a fleet-authored PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeDecision {
    /// Green, small, and verified low-risk — safe to auto-merge.
    AutoMerge,
    /// Landable but risky (large diff or unverified) — route to a human. The
    /// string is the reason, surfaced to the operator.
    HoldForReview(String),
    /// Not landable as-is (CI red, checks incomplete, or conflicts). The string
    /// is the blocking reason.
    Block(String),
}

/// Diff size (files changed) at/above which a PR is considered "large" and is
/// routed to human review rather than auto-merged, even when fully green.
pub const LARGE_DIFF_FILE_THRESHOLD: usize = 20;

/// Decide what to do with a fleet-authored PR from its CI + risk signals.
///
/// Policy (in order):
/// 1. **Block** if CI didn't pass, not every check reported success
///    (`ci_pass_count < ci_total`), or the branch has merge conflicts.
/// 2. **HoldForReview** if the diff is large (`files_changed >=`
///    [`LARGE_DIFF_FILE_THRESHOLD`]) or the verify gate wasn't green — risky or
///    unverified changes get a human.
/// 3. **AutoMerge** otherwise — green, small, and verify-green.
///
/// Pure: no I/O, so the merge-drain tick can unit-test its own wiring against
/// this and the operator can reason about exactly when the fleet merges itself.
pub fn pr_merge_decision(
    ci_passed: bool,
    ci_total: usize,
    ci_pass_count: usize,
    has_conflicts: bool,
    files_changed: usize,
    is_verify_green: bool,
) -> MergeDecision {
    // 1. Hard blocks — never land these.
    if has_conflicts {
        return MergeDecision::Block("merge conflicts with base branch".to_string());
    }
    if !ci_passed {
        return MergeDecision::Block("CI did not pass".to_string());
    }
    if ci_pass_count < ci_total {
        return MergeDecision::Block(format!(
            "CI incomplete: {ci_pass_count}/{ci_total} checks passed"
        ));
    }

    // 2. Landable but risky — hand to a human.
    if files_changed >= LARGE_DIFF_FILE_THRESHOLD {
        return MergeDecision::HoldForReview(format!(
            "large diff ({files_changed} files) — needs human review"
        ));
    }
    if !is_verify_green {
        return MergeDecision::HoldForReview(
            "verify gate not green — needs human review".to_string(),
        );
    }

    // 3. Green, small, verified — safe to auto-merge.
    MergeDecision::AutoMerge
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn green_small_verified_auto_merges() {
        assert_eq!(
            pr_merge_decision(true, 6, 6, false, 3, true),
            MergeDecision::AutoMerge
        );
    }

    #[test]
    fn ci_red_is_blocked() {
        assert!(matches!(
            pr_merge_decision(false, 6, 2, false, 3, true),
            MergeDecision::Block(_)
        ));
    }

    #[test]
    fn incomplete_ci_is_blocked() {
        // ci_passed=true but not every check reported success yet.
        assert!(matches!(
            pr_merge_decision(true, 6, 5, false, 3, true),
            MergeDecision::Block(_)
        ));
    }

    #[test]
    fn conflicts_are_blocked_even_when_green() {
        assert!(matches!(
            pr_merge_decision(true, 6, 6, true, 3, true),
            MergeDecision::Block(_)
        ));
    }

    #[test]
    fn large_diff_holds_for_review() {
        assert!(matches!(
            pr_merge_decision(true, 6, 6, false, LARGE_DIFF_FILE_THRESHOLD, true),
            MergeDecision::HoldForReview(_)
        ));
        assert!(matches!(
            pr_merge_decision(true, 6, 6, false, 50, true),
            MergeDecision::HoldForReview(_)
        ));
    }

    #[test]
    fn unverified_holds_for_review() {
        // Green + small but verify gate not green → human.
        assert!(matches!(
            pr_merge_decision(true, 6, 6, false, 3, false),
            MergeDecision::HoldForReview(_)
        ));
    }

    #[test]
    fn block_takes_precedence_over_hold() {
        // Large diff AND red CI → Block wins (safety first).
        assert!(matches!(
            pr_merge_decision(false, 6, 1, false, 100, false),
            MergeDecision::Block(_)
        ));
    }

    #[test]
    fn just_under_large_threshold_can_auto_merge() {
        assert_eq!(
            pr_merge_decision(true, 6, 6, false, LARGE_DIFF_FILE_THRESHOLD - 1, true),
            MergeDecision::AutoMerge
        );
    }
}
