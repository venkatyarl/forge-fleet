//! Process-local cache for precomputed work-item context.
//!
//! Context assembly may involve several memory and code-graph lookups. This
//! cache lets dispatchers reuse the assembled prompt while keeping entries
//! bounded and short-lived.

use std::time::{Duration, Instant};

use dashmap::DashMap;

const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);
const DEFAULT_MAX_ENTRIES: usize = 1_024;

#[derive(Debug, Clone)]
struct CachedContext {
    context: String,
    inserted_at: Instant,
}

/// Thread-safe cache keyed by work-item id.
#[derive(Debug)]
pub struct WorkItemContextCache {
    entries: DashMap<String, CachedContext>,
    ttl: Duration,
    max_entries: usize,
}

impl WorkItemContextCache {
    /// Create a cache with the given entry lifetime and capacity.
    ///
    /// A capacity of zero disables storage, which is useful for callers that
    /// want to turn caching off without branching at each call site.
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
            max_entries,
        }
    }

    /// Return a cached context when it is still fresh.
    pub fn get(&self, work_item_id: &str) -> Option<String> {
        let entry = self.entries.get(work_item_id)?;
        if entry.inserted_at.elapsed() < self.ttl {
            return Some(entry.context.clone());
        }
        drop(entry);
        self.entries.remove(work_item_id);
        None
    }

    /// Store or replace a precomputed context for a work item.
    pub fn insert(&self, work_item_id: impl Into<String>, context: impl Into<String>) {
        if self.max_entries == 0 {
            return;
        }

        self.prune_expired();
        let work_item_id = work_item_id.into();
        if !self.entries.contains_key(&work_item_id) && self.entries.len() >= self.max_entries {
            self.evict_oldest();
        }
        self.entries.insert(
            work_item_id,
            CachedContext {
                context: context.into(),
                inserted_at: Instant::now(),
            },
        );
    }

    /// Invalidate a work item's context, returning whether it was cached.
    pub fn invalidate(&self, work_item_id: &str) -> bool {
        self.entries.remove(work_item_id).is_some()
    }

    /// Remove all cached contexts.
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// Remove expired entries and return the number removed.
    pub fn prune_expired(&self) -> usize {
        let before = self.entries.len();
        let ttl = self.ttl;
        self.entries
            .retain(|_, entry| entry.inserted_at.elapsed() < ttl);
        before.saturating_sub(self.entries.len())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn evict_oldest(&self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|entry| entry.inserted_at)
            .map(|entry| entry.key().clone());
        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }
}

impl Default for WorkItemContextCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL, DEFAULT_MAX_ENTRIES)
    }
}

/// Concise alias for callers where the cached value is already understood to
/// be work-item context.
pub type ContextCache = WorkItemContextCache;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_invalidates_context() {
        let cache = WorkItemContextCache::default();
        cache.insert("wi-1", "assembled context");

        assert_eq!(cache.get("wi-1").as_deref(), Some("assembled context"));
        assert!(cache.invalidate("wi-1"));
        assert!(cache.get("wi-1").is_none());
    }

    #[test]
    fn expired_context_is_not_returned() {
        let cache = WorkItemContextCache::new(Duration::ZERO, 4);
        cache.insert("wi-1", "stale context");

        assert!(cache.get("wi-1").is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let cache = WorkItemContextCache::new(Duration::from_secs(60), 2);
        cache.insert("wi-1", "first");
        cache.insert("wi-2", "second");
        cache.insert("wi-3", "third");

        assert!(cache.get("wi-1").is_none());
        assert_eq!(cache.get("wi-2").as_deref(), Some("second"));
        assert_eq!(cache.get("wi-3").as_deref(), Some("third"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn zero_capacity_disables_storage() {
        let cache = WorkItemContextCache::new(Duration::from_secs(60), 0);
        cache.insert("wi-1", "context");

        assert!(cache.is_empty());
    }
}
