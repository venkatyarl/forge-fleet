use std::collections::HashSet;
use std::sync::Mutex;

use ff_core::Node;
use uuid::Uuid;

/// Tracks whether nodes should be actively synchronized by the orchestrator.
#[derive(Debug, Default)]
pub struct NodeManager {
    offline_safe_nodes: Mutex<HashSet<Uuid>>,
}

impl NodeManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return whether the orchestrator should perform a heavy sync for `node`.
    ///
    /// Offline-autonomous nodes retain their local state and are deliberately
    /// left alone until they rejoin normal orchestration.
    pub fn should_run_heavy_sync(&self, node: &Node) -> bool {
        if node.is_offline_autonomy_enabled {
            self.mark_node_offline_safe(node.id);
            return false;
        }

        true
    }

    /// Mark a node as safe to continue from its local state while offline.
    pub fn mark_node_offline_safe(&self, node_id: Uuid) {
        self.offline_safe_nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(node_id);
    }

    pub fn is_node_offline_safe(&self, node_id: Uuid) -> bool {
        self.offline_safe_nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&node_id)
    }
}
