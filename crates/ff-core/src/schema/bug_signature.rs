//! Pure helpers for identifying recurring, previously healed bugs.

use std::time::Duration;

/// States from which a self-heal operation may be started again.
pub const TERMINAL_SELF_HEAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];

/// Compare two stable bug signatures.
///
/// Empty signatures are treated as missing identifiers rather than as a
/// match. This prevents unrelated unclassified failures from being folded
/// together and re-armed as one bug.
pub fn signatures_match(previous: &str, observed: &str) -> bool {
    !previous.is_empty() && previous == observed
}

/// Return whether `status` represents a self-heal task that is no longer in
/// flight and can therefore be re-armed.
pub fn is_terminal_self_heal_status(status: &str) -> bool {
    TERMINAL_SELF_HEAL_STATUSES.contains(&status)
}

/// Decide whether a newly observed signature should re-arm an existing task.
///
/// A task is eligible only when the stable signatures match, its previous run
/// reached a terminal state, and the configured cooldown has elapsed.
pub fn should_rearm_signature(
    previous: &str,
    observed: &str,
    status: &str,
    elapsed_since_terminal: Duration,
    cooldown: Duration,
) -> bool {
    signatures_match(previous, observed)
        && is_terminal_self_heal_status(status)
        && elapsed_since_terminal >= cooldown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_comparison_requires_same_non_empty_value() {
        assert!(signatures_match("abc123", "abc123"));
        assert!(!signatures_match("abc123", "def456"));
        assert!(!signatures_match("", ""));
    }

    #[test]
    fn healed_signature_rearms_after_cooldown() {
        assert!(should_rearm_signature(
            "abc123",
            "abc123",
            "completed",
            Duration::from_secs(31),
            Duration::from_secs(30),
        ));
    }

    #[test]
    fn active_changed_or_cooling_signatures_do_not_rearm() {
        let cooldown = Duration::from_secs(30);
        assert!(!should_rearm_signature(
            "abc123",
            "abc123",
            "running",
            Duration::from_secs(31),
            cooldown,
        ));
        assert!(!should_rearm_signature(
            "abc123",
            "def456",
            "completed",
            Duration::from_secs(31),
            cooldown,
        ));
        assert!(!should_rearm_signature(
            "abc123",
            "abc123",
            "completed",
            Duration::from_secs(29),
            cooldown,
        ));
    }
}
