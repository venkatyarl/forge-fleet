//! Coordination for concurrent file edits.
//!
//! Each path may be edited by only one owner at a time. Claims use DashMap's
//! atomic entry API so two callers cannot both observe an unclaimed path and
//! then acquire it.

use std::path::{Path, PathBuf};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

/// Describes an edit that could not be scheduled because the path is busy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditConflict {
    pub path: PathBuf,
    pub current_owner: String,
}

impl std::fmt::Display for EditConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is already being edited by {}",
            self.path.display(),
            self.current_owner
        )
    }
}

impl std::error::Error for EditConflict {}

/// Process-local scheduler that prevents overlapping edits to the same path.
#[derive(Debug, Default)]
pub struct EditScheduler {
    active_edits: DashMap<PathBuf, String>,
}

impl EditScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically claim `path` for `owner`.
    pub fn try_begin(
        &self,
        path: impl Into<PathBuf>,
        owner: impl Into<String>,
    ) -> Result<(), EditConflict> {
        let path = path.into();
        match self.active_edits.entry(path) {
            Entry::Vacant(entry) => {
                entry.insert(owner.into());
                Ok(())
            }
            Entry::Occupied(entry) => Err(EditConflict {
                path: entry.key().clone(),
                current_owner: entry.get().clone(),
            }),
        }
    }

    /// Release `path` only when it is still claimed by `owner`.
    ///
    /// Owner checking prevents a delayed completion from releasing a newer
    /// editor's claim.
    pub fn finish(&self, path: impl AsRef<Path>, owner: &str) -> bool {
        self.active_edits
            .remove_if(path.as_ref(), |_, current_owner| current_owner == owner)
            .is_some()
    }

    pub fn is_active(&self, path: impl AsRef<Path>) -> bool {
        self.active_edits.contains_key(path.as_ref())
    }

    pub fn len(&self) -> usize {
        self.active_edits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active_edits.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    #[test]
    fn detects_an_existing_edit() {
        let scheduler = EditScheduler::new();
        scheduler.try_begin("src/lib.rs", "agent-1").unwrap();

        let conflict = scheduler.try_begin("src/lib.rs", "agent-2").unwrap_err();
        assert_eq!(conflict.path, PathBuf::from("src/lib.rs"));
        assert_eq!(conflict.current_owner, "agent-1");
    }

    #[test]
    fn only_one_concurrent_edit_wins() {
        const EDITORS: usize = 8;
        let scheduler = Arc::new(EditScheduler::new());
        let barrier = Arc::new(Barrier::new(EDITORS));

        let handles: Vec<_> = (0..EDITORS)
            .map(|editor| {
                let scheduler = Arc::clone(&scheduler);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    scheduler
                        .try_begin("src/lib.rs", format!("agent-{editor}"))
                        .is_ok()
                })
            })
            .collect();

        let winners = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn only_the_owner_can_finish_an_edit() {
        let scheduler = EditScheduler::new();
        scheduler.try_begin("src/lib.rs", "agent-1").unwrap();

        assert!(!scheduler.finish("src/lib.rs", "agent-2"));
        assert!(scheduler.is_active("src/lib.rs"));
        assert!(scheduler.finish("src/lib.rs", "agent-1"));
        assert!(scheduler.is_empty());
    }
}
