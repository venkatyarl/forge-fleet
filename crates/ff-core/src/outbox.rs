use std::collections::VecDeque;

/// An in-memory FIFO queue for items waiting to be processed.
pub struct LocalOutbox<T> {
    items: VecDeque<T>,
}

impl<T> LocalOutbox<T> {
    /// Creates an empty outbox.
    pub fn new() -> Self {
        Self {
            items: VecDeque::new(),
        }
    }

    /// Adds an item to the back of the queue.
    pub fn push(&mut self, item: T) {
        self.items.push_back(item);
    }

    /// Removes and returns all queued items in FIFO order.
    pub fn drain(&mut self) -> Vec<T> {
        self.items.drain(..).collect()
    }

    /// Returns whether the queue contains no items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<T> Default for LocalOutbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::LocalOutbox;

    #[test]
    fn new_outbox_is_empty() {
        let outbox = LocalOutbox::<i32>::new();

        assert!(outbox.is_empty());
    }

    #[test]
    fn drain_returns_items_in_fifo_order_and_empties_queue() {
        let mut outbox = LocalOutbox::default();
        outbox.push("first");
        outbox.push("second");

        assert_eq!(outbox.drain(), vec!["first", "second"]);
        assert!(outbox.is_empty());
    }
}
