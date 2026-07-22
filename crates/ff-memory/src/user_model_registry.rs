//! Registry of [`GlobalUserModel`] instances spanning every memory realm.
//!
//! Mirrors the [`WorkItemContextCache`](crate::cache::WorkItemContextCache)
//! pattern used elsewhere in this crate: a `DashMap`-backed, process-local
//! store with synchronous upsert/lookup/remove access, rather than a
//! roundtrip to Postgres for every read.

use dashmap::DashMap;

use crate::user_model::{GlobalUserModel, UserModelNode, merge_user_model};
use crate::{NodeId, Realm};

/// Identifies a registered global user model, e.g. a tenant or workspace slug.
pub type GlobalUserKey = String;

/// A stored global user model together with the raw `operates_on` edges used
/// to build it, so cross-realm links can be resolved without recomputing the
/// merge.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalUserEntry {
    pub model: GlobalUserModel,
    pub operates_on: Vec<(NodeId, NodeId)>,
}

/// Thread-safe, process-local store of [`GlobalUserModel`] instances, keyed
/// by an arbitrary caller-supplied identifier.
#[derive(Debug, Default)]
pub struct GlobalUserRegistry {
    entries: DashMap<GlobalUserKey, GlobalUserEntry>,
}

impl GlobalUserRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the global user model for `key` in the designated
    /// global store, returning the model that was stored.
    pub fn upsert_global_user(
        &self,
        key: impl Into<GlobalUserKey>,
        model: GlobalUserModel,
    ) -> GlobalUserModel {
        let key = key.into();
        let operates_on = self
            .entries
            .get(&key)
            .map(|entry| entry.operates_on.clone())
            .unwrap_or_default();
        self.entries.insert(
            key,
            GlobalUserEntry {
                model: model.clone(),
                operates_on,
            },
        );
        model
    }

    /// Look up the global user model stored for `key`.
    pub fn get(&self, key: &str) -> Option<GlobalUserModel> {
        self.entries.get(key).map(|entry| entry.model.clone())
    }

    /// Return the `operates_on` edge references backing the cross-realm
    /// contexts of the model registered under `key`, for `operates_on`
    /// linking by callers that need to walk those edges directly.
    pub fn resolve_user_edges(&self, key: &str) -> Vec<(NodeId, NodeId)> {
        self.entries
            .get(key)
            .map(|entry| entry.operates_on.clone())
            .unwrap_or_default()
    }

    /// Recompute the global user model for `key` from raw realm/node/edge
    /// inputs spanning multiple realms, write the merged result to the
    /// global store, and return the `operates_on` edge references used to
    /// link nodes across realms.
    pub fn sync_cross_realm_refs(
        &self,
        key: impl Into<GlobalUserKey>,
        realms: Vec<Realm>,
        nodes: Vec<UserModelNode>,
        operates_on: Vec<(NodeId, NodeId)>,
    ) -> Vec<(NodeId, NodeId)> {
        let model = merge_user_model(realms, nodes, operates_on.clone());
        self.entries.insert(
            key.into(),
            GlobalUserEntry {
                model,
                operates_on: operates_on.clone(),
            },
        );
        operates_on
    }

    /// Remove a registered global user model, returning it if present.
    pub fn remove(&self, key: &str) -> Option<GlobalUserModel> {
        self.entries.remove(key).map(|(_, entry)| entry.model)
    }

    /// List all keys currently tracked by the registry.
    pub fn tracked_keys(&self) -> Vec<GlobalUserKey> {
        self.entries
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::RealmId;
    use crate::user_model::UserModelNode;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn sample_model() -> GlobalUserModel {
        GlobalUserModel {
            realms: vec![],
            nodes: vec![],
            contexts: vec![],
        }
    }

    #[test]
    fn upserts_and_looks_up_by_key() {
        let registry = GlobalUserRegistry::new();
        assert!(registry.get("tenant-a").is_none());

        registry.upsert_global_user("tenant-a", sample_model());
        assert!(registry.get("tenant-a").is_some());
        assert_eq!(registry.len(), 1);

        // Upsert replaces rather than duplicates.
        registry.upsert_global_user("tenant-a", sample_model());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn removes_registered_entries() {
        let registry = GlobalUserRegistry::new();
        registry.upsert_global_user("tenant-a", sample_model());

        assert!(registry.remove("tenant-a").is_some());
        assert!(registry.get("tenant-a").is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn sync_cross_realm_refs_merges_and_stores_operates_on_edges() {
        let registry = GlobalUserRegistry::new();

        let realm_a = RealmId(id(1));
        let realm_b = RealmId(id(2));
        let node_a = NodeId(id(10));
        let node_b = NodeId(id(11));
        let node_c = NodeId(id(12));

        let realms = vec![
            Realm {
                id: realm_a,
                name: "personal".into(),
            },
            Realm {
                id: realm_b,
                name: "work".into(),
            },
        ];
        let nodes = vec![
            UserModelNode {
                id: node_a,
                realm_id: realm_a,
                content: json!({"a": 1}),
            },
            UserModelNode {
                id: node_b,
                realm_id: realm_b,
                content: json!({"b": 2}),
            },
            UserModelNode {
                id: node_c,
                realm_id: realm_b,
                content: json!({"c": 3}),
            },
        ];
        let operates_on = vec![(node_a, node_b)];

        let returned_edges =
            registry.sync_cross_realm_refs("tenant-a", realms, nodes, operates_on.clone());
        assert_eq!(returned_edges, operates_on);

        let model = registry.get("tenant-a").expect("model stored");
        assert_eq!(model.contexts.len(), 2);
        assert_eq!(model.contexts[0].node_ids, vec![node_a, node_b]);
        assert_eq!(model.contexts[0].realm_ids, vec![realm_a, realm_b]);

        assert_eq!(registry.resolve_user_edges("tenant-a"), operates_on);
        assert!(registry.resolve_user_edges("missing-tenant").is_empty());
    }

    #[test]
    fn tracked_keys_lists_all_registered_users() {
        let registry = GlobalUserRegistry::new();
        registry.upsert_global_user("tenant-a", sample_model());
        registry.upsert_global_user("tenant-b", sample_model());

        let mut keys = registry.tracked_keys();
        keys.sort();
        assert_eq!(keys, vec!["tenant-a".to_string(), "tenant-b".to_string()]);
    }
}
