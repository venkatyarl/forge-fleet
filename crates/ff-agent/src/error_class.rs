//! Stable class tokens for operator-visible failures persisted by `ff-agent`.
//!
//! Keep classification at the persistence boundary: many errors originate in
//! shared helpers, and prefixing only individual `bail!` sites lets unclassified
//! messages leak into `work_items.last_error` and `ff_interactions.error_text`.

pub const ALREADY_IMPLEMENTED: &str = "already_implemented";
pub const AUTH_SUCCESS_SHAPED: &str = "auth_success_shaped";
pub const BACKEND_UNAVAILABLE: &str = "backend_unavailable";
pub const CHILD_FAILED: &str = "child_failed";
pub const CLI_EXECUTION: &str = "cli_execution";
pub const DECOMPOSE_QUALITY_GATE: &str = "decompose_quality_gate";
pub const GIT_WORKTREE_LOCK: &str = "git_worktree_lock";
pub const LEASE_STALLED: &str = "lease_stalled";
pub const MERGE_CONFLICT: &str = "merge_conflict";
pub const MERGE_FAILED: &str = "merge_failed";
pub const MISSING_PR_URL: &str = "missing_pr_url";
pub const NO_DIFF_AFTER_BUILD: &str = "no_diff_after_build";
pub const REVIEW_REJECTED: &str = "review_rejected";
pub const SELF_VERIFY_FAILED: &str = "self_verify_failed";
pub const TASK_EXECUTION: &str = "task_execution";
pub const TIMEOUT: &str = "timeout";
pub const UNKNOWN_FAILURE: &str = "unknown_failure";

pub const ALL: &[&str] = &[
    ALREADY_IMPLEMENTED,
    AUTH_SUCCESS_SHAPED,
    BACKEND_UNAVAILABLE,
    CHILD_FAILED,
    CLI_EXECUTION,
    DECOMPOSE_QUALITY_GATE,
    GIT_WORKTREE_LOCK,
    LEASE_STALLED,
    MERGE_CONFLICT,
    MERGE_FAILED,
    MISSING_PR_URL,
    NO_DIFF_AFTER_BUILD,
    REVIEW_REJECTED,
    SELF_VERIFY_FAILED,
    TASK_EXECUTION,
    TIMEOUT,
    UNKNOWN_FAILURE,
];

/// Add a canonical class token without ever double-prefixing a message.
pub fn prefix(class: &'static str, message: impl AsRef<str>) -> String {
    let message = message.as_ref();
    if message.starts_with("class=") {
        message.to_string()
    } else {
        format!("class={class}: {message}")
    }
}

/// Classify errors that reach a shared persistence boundary without a token.
pub fn classify(message: impl AsRef<str>) -> String {
    let message = message.as_ref();
    if message.starts_with("class=") {
        return message.to_string();
    }
    let lower = message.to_ascii_lowercase();
    let class = if lower.contains("success-shaped auth")
        || (lower.contains("authenticate") && lower.contains("exit 0"))
    {
        AUTH_SUCCESS_SHAPED
    } else if lower.contains("timed out") || lower.contains("timeout") {
        TIMEOUT
    } else if lower.contains("worktree") && (lower.contains("lock") || lower.contains("locked")) {
        GIT_WORKTREE_LOCK
    } else if lower.contains("no diff")
        || lower.contains("produced no changes")
        || lower.contains("empty stdout")
    {
        NO_DIFF_AFTER_BUILD
    } else if lower.contains("review") && (lower.contains("reject") || lower.contains("failed")) {
        REVIEW_REJECTED
    } else if lower.contains("self-verif") || lower.contains("cargo check") {
        SELF_VERIFY_FAILED
    } else if lower.contains("backend") || lower.contains(" on path") {
        BACKEND_UNAVAILABLE
    } else if lower.contains("lease") && (lower.contains("stall") || lower.contains("expired")) {
        LEASE_STALLED
    } else if lower.contains("merge conflict") || lower.contains("conflicted with") {
        MERGE_CONFLICT
    } else {
        UNKNOWN_FAILURE
    };
    prefix(class, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_tokens_are_unique_snake_case() {
        let mut unique = HashSet::new();
        for token in ALL {
            assert!(unique.insert(*token), "duplicate error class: {token}");
            assert!(!token.is_empty());
            assert!(token.as_bytes()[0].is_ascii_lowercase());
            assert!(
                token
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            );
            assert!(!token.ends_with('_'));
            assert!(!token.contains("__"));
        }
    }

    #[test]
    fn classification_prefixes_once() {
        let classified = classify("backend produced no diff");
        assert_eq!(
            classified,
            "class=no_diff_after_build: backend produced no diff"
        );
        assert_eq!(classify(&classified), classified);
        assert_eq!(
            classify("claude: success-shaped auth failure (exit 0)"),
            "class=auth_success_shaped: claude: success-shaped auth failure (exit 0)"
        );
    }
}
