//! Slot-scoped execution coordination.

use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;

/// Registry of edit locks keyed by execution slot.
///
/// A slot may run tool calls concurrently, but its file edits are
/// read-modify-write operations and must not overlap. Separate slots retain
/// separate locks so unrelated builds can continue in parallel.
static EDIT_LOCKS: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Return the edit-serialization lock for `slot_id`.
pub fn slot_edit_lock(slot_id: &str) -> Arc<Mutex<()>> {
    EDIT_LOCKS
        .lock()
        .expect("slot edit-lock registry poisoned")
        .entry(slot_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Remove a slot's edit lock after its execution has ended.
pub fn clear_slot_edit_lock(slot_id: &str) {
    EDIT_LOCKS
        .lock()
        .expect("slot edit-lock registry poisoned")
        .remove(slot_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn edits_in_the_same_slot_are_serialized() {
        let slot_id = uuid::Uuid::new_v4().to_string();
        let first_lock = slot_edit_lock(&slot_id);
        let second_lock = slot_edit_lock(&slot_id);
        let first_guard = first_lock.lock().await;

        let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _second_guard = second_lock.lock().await;
            entered_tx.send(()).unwrap();
        });

        tokio::task::yield_now().await;
        assert!(matches!(
            entered_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        drop(first_guard);
        entered_rx.await.unwrap();
        waiter.await.unwrap();
        clear_slot_edit_lock(&slot_id);
    }

    #[tokio::test]
    async fn edits_in_different_slots_do_not_block_each_other() {
        let first_slot = uuid::Uuid::new_v4().to_string();
        let second_slot = uuid::Uuid::new_v4().to_string();
        let first_lock = slot_edit_lock(&first_slot);
        let _first_guard = first_lock.lock().await;

        let second_lock = slot_edit_lock(&second_slot);
        assert!(second_lock.try_lock().is_ok());

        clear_slot_edit_lock(&first_slot);
        clear_slot_edit_lock(&second_slot);
    }
}
