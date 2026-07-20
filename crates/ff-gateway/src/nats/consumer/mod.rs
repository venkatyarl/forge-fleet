//! JetStream consumers owned by the gateway.

use tokio::sync::Mutex;

pub mod alerts;
pub mod scheduler;

/// Serializes message handling for the gateway's durable consumer.
///
/// Keeping the lock at the consumer module boundary also protects against
/// multiple in-process runners attaching to the same durable consumer.
pub(super) static MESSAGE_PROCESSING_LOCK: Mutex<()> = Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn message_processing_is_serialized() {
        let first_guard = MESSAGE_PROCESSING_LOCK.lock().await;
        let (attempting_tx, attempting_rx) = tokio::sync::oneshot::channel();
        let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();

        let waiter = tokio::spawn(async move {
            attempting_tx.send(()).unwrap();
            let _second_guard = MESSAGE_PROCESSING_LOCK.lock().await;
            entered_tx.send(()).unwrap();
        });

        attempting_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            entered_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        drop(first_guard);
        entered_rx.await.unwrap();
        waiter.await.unwrap();
    }
}
