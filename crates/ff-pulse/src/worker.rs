//! Slot-level locking for ff-pulse workers.
//!
//! A [`Worker`] owns a fixed number of independent slots.  The [`Worker::edit`]
//! helper guarantees that only one closure runs against a given slot at any
//! time, so concurrent work for different slots proceeds in parallel while
//! work for the same slot is serialized.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};

/// A pool of independent slots, each protected by its own lock.
#[derive(Clone, Debug)]
pub struct Worker<T> {
    slots: Arc<Vec<Mutex<T>>>,
}

impl<T: Clone> Worker<T> {
    /// Create a worker that owns `slot_count` slots, all initialized to `value`.
    pub fn new(slot_count: usize, value: T) -> Self {
        Self::from_values(std::iter::repeat(value).take(slot_count))
    }
}

impl<T> Worker<T> {
    /// Create a worker from an explicit list of slot values.
    pub fn from_values(values: impl IntoIterator<Item = T>) -> Self {
        let slots: Vec<_> = values.into_iter().map(Mutex::new).collect();
        Self {
            slots: Arc::new(slots),
        }
    }

    /// Number of slots in this worker.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Run `f` with exclusive access to slot `slot_id`.
    ///
    /// Returns `None` if `slot_id` is out of range.  Because each slot has its
    /// own mutex, edits to different slots can run concurrently, but edits to
    /// the same slot are serialized so that only one edit per slot can occur
    /// at a time.
    pub fn edit<F, R>(&self, slot_id: usize, f: F) -> Option<R>
    where
        F: FnOnce(&mut T) -> R,
    {
        let slot = self.slots.get(slot_id)?;
        let mut guard = slot.lock().ok()?;
        Some(f(&mut *guard))
    }

    /// Acquire exclusive access to slot `slot_id`, returning a guard.
    ///
    /// Returns `None` if `slot_id` is out of range.
    pub fn lock(&self, slot_id: usize) -> Option<SlotGuard<'_, T>> {
        let slot = self.slots.get(slot_id)?;
        let guard = slot.lock().ok()?;
        Some(SlotGuard { guard })
    }
}

/// Exclusive guard for a worker slot.
pub struct SlotGuard<'a, T> {
    guard: MutexGuard<'a, T>,
}

impl<T> Deref for SlotGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &*self.guard
    }
}

impl<T> DerefMut for SlotGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut *self.guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn edit_returns_none_for_out_of_range_slot() {
        let worker = Worker::new(2, 0i32);
        assert!(worker.edit(2, |v| *v += 1).is_none());
        assert!(worker.lock(2).is_none());
    }

    #[test]
    fn edit_serializes_same_slot() {
        let worker = Worker::new(1, 0i32);
        let mut handles = Vec::new();

        for _ in 0..8 {
            let w = worker.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    w.edit(0, |n| *n += 1).unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(worker.edit(0, |n| *n).unwrap(), 8000);
    }

    #[test]
    fn edit_allows_concurrent_access_to_different_slots() {
        // If a single global lock protected the worker, the two closures
        // would reach the barrier one at a time and the test would time out.
        let worker = Worker::new(2, ());
        let barrier = Arc::new(Barrier::new(2));

        let (tx, rx) = mpsc::channel();

        for slot in 0..2 {
            let w = worker.clone();
            let b = Arc::clone(&barrier);
            let tx = tx.clone();
            thread::spawn(move || {
                w.edit(slot, |_| {
                    b.wait();
                })
                .unwrap();
                tx.send(slot).unwrap();
            });
        }

        // Both slots should finish because they do not block each other.
        let mut seen = Vec::new();
        for _ in 0..2 {
            let slot = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            seen.push(slot);
        }
        seen.sort();
        assert_eq!(seen, vec![0, 1]);
    }

    #[test]
    fn lock_guard_deref_mutates_slot() {
        let worker = Worker::new(2, 0i32);
        {
            let mut guard = worker.lock(1).unwrap();
            *guard += 7;
        }
        assert_eq!(worker.edit(1, |n| *n).unwrap(), 7);
        assert_eq!(worker.edit(0, |n| *n).unwrap(), 0);
    }
}
