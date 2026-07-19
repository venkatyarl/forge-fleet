//! Alert deduplication state.
//!
//! This module provides a small, in-memory TTL store for suppressing repeated
//! alerts. It is safe to share between alert producers.

use dashmap::DashMap;
use std::time::{Duration, Instant};

/// Tracks recently emitted alert keys for a fixed time-to-live (TTL).
#[derive(Debug)]
pub struct AlertDeduplicationState {
    ttl: Duration,
    alerts: DashMap<String, Instant>,
}

impl AlertDeduplicationState {
    /// Create empty deduplication state using `ttl` as the suppression window.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            alerts: DashMap::new(),
        }
    }

    /// Return `true` when an alert should be emitted and record it as seen.
    ///
    /// The first occurrence of a key is emitted. Further occurrences are
    /// suppressed until the TTL has elapsed, at which point the key is renewed.
    pub fn should_emit(&self, key: impl Into<String>) -> bool {
        self.should_emit_at(key.into(), Instant::now())
    }

    /// Return whether `key` is currently inside its deduplication window.
    ///
    /// Unlike [`Self::should_emit`], this method does not update the state.
    pub fn is_duplicate(&self, key: &str) -> bool {
        self.is_duplicate_at(key, Instant::now())
    }

    /// Record an alert occurrence without checking its current state.
    pub fn record(&self, key: impl Into<String>) {
        self.alerts.insert(key.into(), Instant::now());
    }

    /// Forget a key immediately. Returns whether it was present.
    pub fn remove(&self, key: &str) -> bool {
        self.alerts.remove(key).is_some()
    }

    /// Remove expired keys and return the number removed.
    pub fn cleanup_expired(&self) -> usize {
        self.cleanup_expired_at(Instant::now())
    }

    /// Number of tracked keys, including entries not yet lazily cleaned up.
    pub fn len(&self) -> usize {
        self.alerts.len()
    }

    /// Return whether no alert keys are currently tracked.
    pub fn is_empty(&self) -> bool {
        self.alerts.is_empty()
    }

    fn should_emit_at(&self, key: String, now: Instant) -> bool {
        use dashmap::mapref::entry::Entry;

        match self.alerts.entry(key) {
            Entry::Occupied(mut entry) => {
                if now
                    .checked_duration_since(*entry.get())
                    .is_none_or(|age| age < self.ttl)
                {
                    false
                } else {
                    entry.insert(now);
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(now);
                true
            }
        }
    }

    fn is_duplicate_at(&self, key: &str, now: Instant) -> bool {
        self.alerts.get(key).is_some_and(|seen_at| {
            now.checked_duration_since(*seen_at)
                .is_none_or(|age| age < self.ttl)
        })
    }

    fn cleanup_expired_at(&self, now: Instant) -> usize {
        let before = self.alerts.len();
        self.alerts.retain(|_, seen_at| {
            now.checked_duration_since(*seen_at)
                .is_none_or(|age| age < self.ttl)
        });
        before - self.alerts.len()
    }
}

impl Default for AlertDeduplicationState {
    fn default() -> Self {
        Self::new(Duration::from_secs(300))
    }
}

/// Short form retained for callers that prefer a concise state type name.
pub type AlertDedupState = AlertDeduplicationState;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_duplicates_until_ttl_expires() {
        let state = AlertDeduplicationState::new(Duration::from_secs(60));
        let start = Instant::now();

        assert!(state.should_emit_at("node-down:a".into(), start));
        assert!(!state.should_emit_at("node-down:a".into(), start + Duration::from_secs(59)));
        assert!(state.should_emit_at("node-down:a".into(), start + Duration::from_secs(60)));
        assert!(!state.should_emit_at("node-down:a".into(), start + Duration::from_secs(61)));
    }

    #[test]
    fn tracks_keys_independently() {
        let state = AlertDeduplicationState::new(Duration::from_secs(60));
        let now = Instant::now();

        assert!(state.should_emit_at("alert:a".into(), now));
        assert!(state.should_emit_at("alert:b".into(), now));
        assert!(!state.should_emit_at("alert:a".into(), now));
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn removes_keys_and_cleans_up_expired_entries() {
        let state = AlertDeduplicationState::new(Duration::from_secs(10));
        let start = Instant::now();
        state.should_emit_at("expired".into(), start);
        state.should_emit_at("current".into(), start + Duration::from_secs(8));

        assert_eq!(state.cleanup_expired_at(start + Duration::from_secs(10)), 1);
        assert!(!state.is_duplicate_at("expired", start + Duration::from_secs(10)));
        assert!(state.is_duplicate_at("current", start + Duration::from_secs(10)));
        assert!(state.remove("current"));
        assert!(state.is_empty());
    }

    #[test]
    fn zero_ttl_never_suppresses() {
        let state = AlertDeduplicationState::new(Duration::ZERO);
        let now = Instant::now();

        assert!(state.should_emit_at("alert".into(), now));
        assert!(state.should_emit_at("alert".into(), now));
        assert!(!state.is_duplicate_at("alert", now));
    }
}
