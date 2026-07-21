//! Connectivity-aware coordinator primitives.
//!
//! The coordinator follows connection-state updates and constrains inference
//! to this computer while the fleet is offline or degraded. Actions that need
//! fleet connectivity are retained in a FIFO outbox for replay after recovery.

use std::{collections::VecDeque, sync::Arc};

use ff_core::schema::state::ConnectionState;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::inference_router::InferenceRouter;

/// An action that must be replayed when fleet connectivity returns.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingAction {
    pub kind: String,
    pub payload: Value,
}

/// Process-local FIFO for actions deferred during local mode.
#[derive(Debug, Default)]
pub struct LocalOutbox {
    pending: Mutex<VecDeque<PendingAction>>,
}

impl LocalOutbox {
    pub async fn push(&self, action: PendingAction) {
        self.pending.lock().await.push_back(action);
    }

    pub async fn pop(&self) -> Option<PendingAction> {
        self.pending.lock().await.pop_front()
    }

    pub async fn len(&self) -> usize {
        self.pending.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.pending.lock().await.is_empty()
    }
}

/// Local-only view of the normal inference router.
#[derive(Clone, Debug)]
pub struct LocalLlmRouter {
    inner: Arc<InferenceRouter>,
}

impl LocalLlmRouter {
    pub fn new(inner: Arc<InferenceRouter>) -> Self {
        Self { inner }
    }

    pub async fn active_url(&self) -> Option<String> {
        self.inner.active_local_url().await
    }
}

/// Selects fleet or local behavior from live connection-state updates.
#[derive(Clone)]
pub struct Coordinator {
    connection_state: Arc<RwLock<ConnectionState>>,
    fleet_router: Arc<InferenceRouter>,
    local_router: LocalLlmRouter,
    outbox: Arc<LocalOutbox>,
}

impl Coordinator {
    pub fn new(fleet_router: Arc<InferenceRouter>, initial_state: ConnectionState) -> Self {
        Self {
            connection_state: Arc::new(RwLock::new(initial_state)),
            local_router: LocalLlmRouter::new(Arc::clone(&fleet_router)),
            fleet_router,
            outbox: Arc::new(LocalOutbox::default()),
        }
    }

    /// Listen until cancellation or until all connection-state senders close.
    pub async fn listen(
        &self,
        mut states: watch::Receiver<ConnectionState>,
        cancel: CancellationToken,
    ) {
        let initial_state = *states.borrow();
        self.set_connection_state(initial_state).await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = states.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let state = *states.borrow_and_update();
                    self.set_connection_state(state).await;
                }
            }
        }
    }

    pub async fn set_connection_state(&self, state: ConnectionState) {
        *self.connection_state.write().await = state;
    }

    pub async fn is_local_mode(&self) -> bool {
        matches!(
            *self.connection_state.read().await,
            ConnectionState::Offline | ConnectionState::Degraded
        )
    }

    /// Select an LLM route appropriate for the current connectivity state.
    pub async fn llm_url(&self) -> Option<String> {
        if self.is_local_mode().await {
            self.local_router.active_url().await
        } else {
            self.fleet_router.active_url().await
        }
    }

    /// Queue an action when connectivity prevents immediate fleet dispatch.
    /// Returns the action unchanged when it can be dispatched immediately.
    pub async fn route_action(&self, action: PendingAction) -> Option<PendingAction> {
        if self.is_local_mode().await {
            self.outbox.push(action).await;
            None
        } else {
            Some(action)
        }
    }

    pub fn outbox(&self) -> Arc<LocalOutbox> {
        Arc::clone(&self.outbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_router::RouterEndpoint;

    fn endpoint(url: &str, is_local: bool) -> RouterEndpoint {
        RouterEndpoint {
            url: url.into(),
            model_id: "test".into(),
            label: url.into(),
            supports_tools: true,
            tier: if is_local { 1 } else { 4 },
            is_local,
            n_ctx: Some(32_768),
        }
    }

    fn coordinator(state: ConnectionState) -> Coordinator {
        Coordinator::new(
            Arc::new(InferenceRouter::new(vec![
                endpoint("http://remote", false),
                endpoint("http://local", true),
            ])),
            state,
        )
    }

    #[tokio::test]
    async fn offline_and_degraded_use_only_local_llm() {
        for state in [ConnectionState::Offline, ConnectionState::Degraded] {
            let coordinator = coordinator(state);
            assert!(coordinator.is_local_mode().await);
            assert_eq!(coordinator.llm_url().await.as_deref(), Some("http://local"));
        }
    }

    #[tokio::test]
    async fn listener_switches_back_to_fleet_mode() {
        let coordinator = coordinator(ConnectionState::Online);
        let (tx, rx) = watch::channel(ConnectionState::Offline);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let coordinator = coordinator.clone();
            let cancel = cancel.clone();
            async move { coordinator.listen(rx, cancel).await }
        });

        tokio::task::yield_now().await;
        assert!(coordinator.is_local_mode().await);
        tx.send(ConnectionState::Online).unwrap();
        tokio::task::yield_now().await;
        assert!(!coordinator.is_local_mode().await);

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn local_mode_queues_actions_in_fifo_order() {
        let coordinator = coordinator(ConnectionState::Degraded);
        for kind in ["first", "second"] {
            assert!(
                coordinator
                    .route_action(PendingAction {
                        kind: kind.into(),
                        payload: Value::Null,
                    })
                    .await
                    .is_none()
            );
        }

        let outbox = coordinator.outbox();
        assert_eq!(outbox.len().await, 2);
        assert_eq!(outbox.pop().await.unwrap().kind, "first");
        assert_eq!(outbox.pop().await.unwrap().kind, "second");
    }

    #[tokio::test]
    async fn online_actions_are_ready_for_immediate_dispatch() {
        let coordinator = coordinator(ConnectionState::Online);
        let action = PendingAction {
            kind: "sync".into(),
            payload: Value::Null,
        };

        assert_eq!(coordinator.route_action(action.clone()).await, Some(action));
        assert!(coordinator.outbox().is_empty().await);
    }
}
