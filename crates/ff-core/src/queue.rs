//! In-memory priority queue for scheduling work by priority.
//!
//! Backed by a binary heap; the highest-priority item is always accessible
//! in O(1) and removable in O(log n). Items with equal priority are returned
//! in FIFO order.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::Result;

/// An entry in the priority queue.
///
/// Ordering is determined by `priority` (higher first) and `sequence` (lower
/// first), so the wrapped `value` does not need to implement any comparison
/// traits. Equality follows the same ordering keys.
#[derive(Debug, Clone)]
pub struct QueueItem<T> {
    /// Larger values are dequeued first.
    pub priority: i64,

    /// Insertion sequence used to break ties in FIFO order.
    pub sequence: u64,

    /// The queued value.
    pub value: T,
}

impl<T> QueueItem<T> {
    /// Create a new queue item.
    pub fn new(priority: i64, sequence: u64, value: T) -> Self {
        Self {
            priority,
            sequence,
            value,
        }
    }
}

impl<T> PartialEq for QueueItem<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl<T> Eq for QueueItem<T> {}

impl<T> PartialOrd for QueueItem<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for QueueItem<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first; for equal priority, lower sequence first (FIFO).
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

/// A priority queue ordered by `priority` (highest first).
///
/// Items with equal priority are handled in FIFO order.
#[derive(Debug, Clone, Default)]
pub struct PriorityQueue<T> {
    heap: BinaryHeap<QueueItem<T>>,
    next_sequence: u64,
}

impl<T> PriorityQueue<T> {
    /// Create an empty priority queue.
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_sequence: 0,
        }
    }

    /// Create an empty priority queue with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            next_sequence: 0,
        }
    }

    /// Return the number of items in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Return true if the queue contains no items.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Insert `value` with `priority`.
    ///
    /// Higher priority values are removed before lower priority values.
    pub fn insert(&mut self, value: T, priority: i64) {
        let item = QueueItem::new(priority, self.next_sequence, value);
        self.next_sequence += 1;
        self.heap.push(item);
    }

    /// Peek at the highest-priority item without removing it.
    ///
    /// Returns an error if the queue is empty.
    pub fn peek(&self) -> Result<&QueueItem<T>> {
        self.heap
            .peek()
            .ok_or_else(|| crate::ForgeFleetError::QueueEmpty)
    }

    /// Remove and return the highest-priority item.
    ///
    /// Returns an error if the queue is empty.
    pub fn remove(&mut self) -> Result<QueueItem<T>> {
        self.heap
            .pop()
            .ok_or_else(|| crate::ForgeFleetError::QueueEmpty)
    }

    /// Remove all items and return them in priority order.
    pub fn drain(&mut self) -> Vec<QueueItem<T>> {
        let mut out = Vec::with_capacity(self.heap.len());
        while let Ok(item) = self.remove() {
            out.push(item);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_queue_is_empty() {
        let pq: PriorityQueue<i32> = PriorityQueue::new();
        assert!(pq.is_empty());
        assert_eq!(pq.len(), 0);
    }

    #[test]
    fn insert_and_peek() {
        let mut pq = PriorityQueue::new();
        pq.insert("low", 1);
        pq.insert("high", 10);
        assert_eq!(pq.peek().map(|i| i.value).unwrap(), "high");
        assert_eq!(pq.len(), 2);
    }

    #[test]
    fn remove_returns_highest_priority() {
        let mut pq = PriorityQueue::new();
        pq.insert("a", 5);
        pq.insert("b", 100);
        pq.insert("c", 5);
        assert_eq!(pq.remove().map(|i| i.value).unwrap(), "b");
        assert_eq!(pq.len(), 2);
    }

    #[test]
    fn fifo_for_equal_priority() {
        let mut pq = PriorityQueue::new();
        pq.insert("first", 1);
        pq.insert("second", 1);
        assert_eq!(pq.remove().map(|i| i.value).unwrap(), "first");
        assert_eq!(pq.remove().map(|i| i.value).unwrap(), "second");
    }

    #[test]
    fn peek_empty_errors() {
        let pq: PriorityQueue<i32> = PriorityQueue::new();
        assert!(matches!(pq.peek(), Err(crate::ForgeFleetError::QueueEmpty)));
    }

    #[test]
    fn remove_empty_errors() {
        let mut pq: PriorityQueue<i32> = PriorityQueue::new();
        assert!(matches!(
            pq.remove(),
            Err(crate::ForgeFleetError::QueueEmpty)
        ));
    }

    #[test]
    fn drain_returns_priority_order() {
        let mut pq = PriorityQueue::new();
        pq.insert("low", 1);
        pq.insert("medium", 5);
        pq.insert("high", 10);

        let values: Vec<&str> = pq.drain().into_iter().map(|i| i.value).collect();
        assert_eq!(values, vec!["high", "medium", "low"]);
        assert!(pq.is_empty());
    }

    #[test]
    fn with_capacity_does_not_panic() {
        let mut pq = PriorityQueue::with_capacity(16);
        pq.insert(42, 1);
        assert_eq!(pq.remove().map(|i| i.value).unwrap(), 42);
    }
}
