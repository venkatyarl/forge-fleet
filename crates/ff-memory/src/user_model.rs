use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;

use crate::{MemoryStore, MemoryStoreError, NodeId, Realm, RealmId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserModelNode {
    pub id: NodeId,
    pub realm_id: RealmId,
    pub content: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserModelContext {
    pub node_ids: Vec<NodeId>,
    pub realm_ids: Vec<RealmId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserModel {
    pub realms: Vec<Realm>,
    pub nodes: Vec<UserModelNode>,
    pub contexts: Vec<UserModelContext>,
}

/// The deduplicated, cross-realm user model as persisted by
/// [`crate::user_model_registry::GlobalUserRegistry`].
pub type GlobalUserModel = UserModel;

#[derive(FromRow)]
struct RealmRow {
    id: uuid::Uuid,
    name: String,
}

#[derive(FromRow)]
struct NodeRow {
    id: uuid::Uuid,
    realm_id: uuid::Uuid,
    content: Value,
}

#[derive(FromRow)]
struct EdgeRow {
    source_node_id: uuid::Uuid,
    target_node_id: uuid::Uuid,
}

impl MemoryStore {
    /// Builds the deduplicated user model spanning every memory realm.
    pub async fn build_global_user_model(&self) -> Result<UserModel, MemoryStoreError> {
        let realms = sqlx::query_as::<_, RealmRow>("SELECT id, name FROM memory_realms")
            .fetch_all(self.pool())
            .await?
            .into_iter()
            .map(|row| Realm {
                id: RealmId(row.id),
                name: row.name,
            })
            .collect();

        let nodes = sqlx::query_as::<_, NodeRow>("SELECT id, realm_id, content FROM memory_nodes")
            .fetch_all(self.pool())
            .await?
            .into_iter()
            .map(|row| UserModelNode {
                id: NodeId(row.id),
                realm_id: RealmId(row.realm_id),
                content: row.content,
            })
            .collect();

        let operates_on = sqlx::query_as::<_, EdgeRow>(
            "SELECT source_node_id, target_node_id FROM memory_edges WHERE edge_type = 'operates_on'",
        )
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|row| (NodeId(row.source_node_id), NodeId(row.target_node_id)))
        .collect();

        Ok(merge_user_model(realms, nodes, operates_on))
    }
}

pub(crate) fn merge_user_model(
    realms: Vec<Realm>,
    nodes: Vec<UserModelNode>,
    operates_on: Vec<(NodeId, NodeId)>,
) -> UserModel {
    let mut realms_by_id = HashMap::new();
    for realm in realms {
        realms_by_id.entry(realm.id).or_insert(realm);
    }

    let mut nodes_by_id = HashMap::new();
    for node in nodes {
        nodes_by_id.entry(node.id).or_insert(node);
    }

    let mut adjacency: HashMap<NodeId, HashSet<NodeId>> = nodes_by_id
        .keys()
        .copied()
        .map(|id| (id, HashSet::new()))
        .collect();
    for (source, target) in operates_on {
        if source == target
            || !nodes_by_id.contains_key(&source)
            || !nodes_by_id.contains_key(&target)
        {
            continue;
        }
        adjacency.entry(source).or_default().insert(target);
        adjacency.entry(target).or_default().insert(source);
    }

    let mut unseen: HashSet<_> = nodes_by_id.keys().copied().collect();
    let mut contexts = Vec::new();
    while let Some(start) = unseen.iter().min_by_key(|id| id.0).copied() {
        let mut queue = VecDeque::from([start]);
        let mut node_ids = Vec::new();
        let mut realm_ids = HashSet::new();
        unseen.remove(&start);

        while let Some(node_id) = queue.pop_front() {
            node_ids.push(node_id);
            realm_ids.insert(nodes_by_id[&node_id].realm_id);
            for neighbor in &adjacency[&node_id] {
                if unseen.remove(neighbor) {
                    queue.push_back(*neighbor);
                }
            }
        }

        node_ids.sort_by_key(|id| id.0);
        let mut realm_ids: Vec<_> = realm_ids.into_iter().collect();
        realm_ids.sort_by_key(|id| id.0);
        contexts.push(UserModelContext {
            node_ids,
            realm_ids,
        });
    }

    let mut realms: Vec<_> = realms_by_id.into_values().collect();
    realms.sort_by_key(|realm| realm.id.0);
    let mut nodes: Vec<_> = nodes_by_id.into_values().collect();
    nodes.sort_by_key(|node| node.id.0);

    UserModel {
        realms,
        nodes,
        contexts,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    #[test]
    fn merges_cross_realm_operates_on_components_and_deduplicates() {
        let realm_a = RealmId(id(1));
        let realm_b = RealmId(id(2));
        let node_a = NodeId(id(10));
        let node_b = NodeId(id(11));
        let node_c = NodeId(id(12));
        let realms = vec![
            Realm {
                id: realm_b,
                name: "work".into(),
            },
            Realm {
                id: realm_a,
                name: "personal".into(),
            },
            Realm {
                id: realm_a,
                name: "duplicate".into(),
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
            UserModelNode {
                id: node_a,
                realm_id: realm_a,
                content: json!({"ignored": true}),
            },
        ];

        let model = merge_user_model(
            realms,
            nodes,
            vec![(node_a, node_b), (node_b, node_a), (node_b, node_b)],
        );

        assert_eq!(model.realms.len(), 2);
        assert_eq!(model.nodes.len(), 3);
        assert_eq!(model.contexts.len(), 2);
        assert_eq!(model.contexts[0].node_ids, vec![node_a, node_b]);
        assert_eq!(model.contexts[0].realm_ids, vec![realm_a, realm_b]);
        assert_eq!(model.contexts[1].node_ids, vec![node_c]);
    }

    #[test]
    fn ignores_dangling_edges() {
        let realm = RealmId(id(1));
        let node = NodeId(id(10));
        let model = merge_user_model(
            vec![Realm {
                id: realm,
                name: "realm".into(),
            }],
            vec![UserModelNode {
                id: node,
                realm_id: realm,
                content: Value::Null,
            }],
            vec![(node, NodeId(id(99)))],
        );

        assert_eq!(
            model.contexts,
            vec![UserModelContext {
                node_ids: vec![node],
                realm_ids: vec![realm]
            }]
        );
    }
}
