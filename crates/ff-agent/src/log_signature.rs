//! Log-signature tracking with recurrence counting.
//!
//! Mirrors the deduplication patterns used for work-item creation and the
//! operator-notification dedup window: a single-flight key (`signature`) is
//! derived from the error, and repeated observations merge into the same entry
//! by incrementing `count` and refreshing `last_seen` — the in-memory
//! equivalent of `ON CONFLICT (signature) DO UPDATE`.
//!
//! Populating `ff_interactions.error_signature` with these signatures feeds
//! the leader's existing self-heal tick (`leader_tick::scan_interaction_errors`),
//! which aggregates recent interaction errors by signature and enqueues novel
//! ones for `self_heal_writer` deferred tasks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// One tracked log/error signature and its recurrence metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSignature {
    /// Stable deduplication key for this signature.
    pub signature: String,
    /// Human-readable canonical text (first line of the source error).
    pub canonical_text: String,
    /// Number of times this signature has been observed.
    pub count: u64,
    /// When this signature was first observed.
    pub first_seen: DateTime<Utc>,
    /// When this signature was last observed.
    pub last_seen: DateTime<Utc>,
}

/// In-memory tracker for log signatures and recurrence counts.
///
/// Thread-safe: concurrent observers lock the inner map briefly, the same way
/// `work_item_scheduler` relies on the DB's partial-unique index for
/// single-flight assignment. The tracker itself is the in-memory equivalent.
#[derive(Debug, Clone)]
pub struct LogSignatureTracker {
    inner: Arc<Mutex<HashMap<String, LogSignature>>>,
    /// Minimum wall-clock interval between re-observations that actually bump
    /// the count. Mirrors `operator_notify_dedup`'s one-hour throttle so a
    /// tight burst of identical log lines only advances the counter once per
    /// window. `Duration::ZERO` counts every observation.
    cooldown: Duration,
    /// Length of the generated hex signature.
    signature_length: usize,
}

impl Default for LogSignatureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LogSignatureTracker {
    /// Build a tracker with the default 16-char SHA-256 signature and no
    /// observation cooldown.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            cooldown: Duration::ZERO,
            signature_length: 16,
        }
    }

    /// Set a cooldown window between counted re-observations of the same
    /// signature.
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// Set the length of generated hex signatures. `len` is clamped to
    /// `[4, 64]` so it remains useful and fits within a full SHA-256 digest.
    pub fn with_signature_length(mut self, len: usize) -> Self {
        self.signature_length = len.clamp(4, 64);
        self
    }

    /// Observe one log/error line and return the merged [`LogSignature`].
    ///
    /// The returned signature is suitable for storing in
    /// `ff_interactions.error_signature` or for operator-alert dedup.
    pub fn observe(&self, text: &str) -> LogSignature {
        let canonical = canonical_error_line(text);
        let signature = compute_signature(&canonical, self.signature_length);
        let now = Utc::now();

        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = guard
            .entry(signature.clone())
            .or_insert_with(|| LogSignature {
                signature: signature.clone(),
                canonical_text: canonical.clone(),
                count: 0,
                first_seen: now,
                last_seen: now,
            });

        // Compare-and-set style merge: only bump the counter if this is the
        // first observation or the cooldown window has elapsed. This gives the
        // same recurrence semantics as `ON CONFLICT ... DO UPDATE SET count =
        // count + 1` without thrashing on a burst of identical lines.
        let should_count = entry.count == 0
            || self.cooldown.is_zero()
            || now
                .signed_duration_since(entry.last_seen)
                .to_std()
                .unwrap_or_default()
                >= self.cooldown;

        if should_count {
            entry.count += 1;
        }
        entry.last_seen = now;
        entry.canonical_text = canonical;
        entry.clone()
    }

    /// Return the tracked signature for `text` without bumping the count.
    pub fn signature_for(&self, text: &str) -> String {
        compute_signature(&canonical_error_line(text), self.signature_length)
    }

    /// Look up a signature without observing it.
    pub fn get(&self, signature: &str) -> Option<LogSignature> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(signature)
            .cloned()
    }

    /// Number of distinct signatures currently tracked.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All tracked signatures, newest `last_seen` first.
    pub fn signatures(&self) -> Vec<LogSignature> {
        let mut all: Vec<_> = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        all.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        all
    }

    /// Drain all tracked signatures, returning them newest first.
    pub fn drain(&self) -> Vec<LogSignature> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut all: Vec<_> = std::mem::take(&mut *guard).into_values().collect();
        all.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        all
    }
}

/// Global process-level tracker used by work-item dispatch to feed recurrence
/// counts into `ff_interactions.error_signature` without threading a tracker
/// through every spawned task. Lazily initialized on first use.
pub fn global_tracker() -> &'static LogSignatureTracker {
    static GLOBAL: OnceLock<LogSignatureTracker> = OnceLock::new();
    GLOBAL.get_or_init(LogSignatureTracker::new)
}

/// Normalize an error string into its canonical dedup line.
///
/// Uses the same rule as `work_item_dispatch::notify_operator_task_failed`:
/// take the first line, trim whitespace, and truncate to 200 characters so
/// variable stack context after the first line does not break dedup.
pub fn canonical_error_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or(text)
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// Compute a stable hex signature from canonical error text.
///
/// Follows the same convention as `ff_core::panic_hook::compute_signature`:
/// SHA-256 of the normalized text, truncated to `len` hex characters.
pub fn compute_signature(text: &str, len: usize) -> String {
    let mut h = Sha256::new();
    h.update(text);
    let full = format!("{:x}", h.finalize());
    full.chars().take(len.clamp(1, 64)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn signature_is_stable_and_short() {
        let a = compute_signature("pool timed out", 16);
        let b = compute_signature("pool timed out", 16);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn canonical_line_truncates_and_skips_context() {
        let text = "pool timed out\nstack line 1\nstack line 2";
        assert_eq!(canonical_error_line(text), "pool timed out");

        let long = "a".repeat(500);
        let canon = canonical_error_line(&long);
        assert_eq!(canon.len(), 200);
    }

    #[test]
    fn observe_counts_and_merges() {
        let tracker = LogSignatureTracker::new();
        let first = tracker.observe("pool timed out");
        assert_eq!(first.count, 1);

        let second = tracker.observe("pool timed out");
        assert_eq!(second.signature, first.signature);
        assert_eq!(second.count, 2);

        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn cooldown_throttles_count() {
        let tracker = LogSignatureTracker::new().with_cooldown(Duration::from_secs(60));
        let first = tracker.observe("rate limited");
        assert_eq!(first.count, 1);

        let second = tracker.observe("rate limited");
        assert_eq!(second.count, 1); // still within cooldown
        assert!(second.last_seen >= first.last_seen);
    }

    #[test]
    fn concurrent_observations_merge_safely() {
        let tracker = LogSignatureTracker::new();
        let mut handles = Vec::new();
        for _ in 0..10 {
            let t = tracker.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    t.observe("concurrent error");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let sig = tracker
            .get(&compute_signature("concurrent error", 16))
            .unwrap();
        assert_eq!(sig.count, 1000);
    }

    #[test]
    fn drain_empties_tracker() {
        let tracker = LogSignatureTracker::new();
        tracker.observe("one");
        tracker.observe("two");
        assert_eq!(tracker.len(), 2);
        let drained = tracker.drain();
        assert_eq!(drained.len(), 2);
        assert!(tracker.is_empty());
    }
}
