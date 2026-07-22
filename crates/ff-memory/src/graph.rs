use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::{EdgeType, MemoryStore, MemoryStoreError, NodeId, Realm, RealmId, UserModelNode};

/// A directed `operates_on` relationship between two memory nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub source_node_id: NodeId,
    pub target_node_id: NodeId,
    pub edge_type: EdgeType,
}

/// The nodes and edges that live entirely within one memory realm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subgraph {
    pub realm: Option<Realm>,
    pub nodes: Vec<UserModelNode>,
    pub edges: Vec<Edge>,
}

/// Which realms [`MemoryStore::find_operates_on_edges`] should traverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphScope {
    /// Only cross-realm edges touching this realm.
    Realm(RealmId),
    /// Cross-realm edges touching any realm.
    AllRealms,
}

#[derive(FromRow)]
struct RealmRow {
    id: uuid::Uuid,
    name: String,
}

#[derive(FromRow)]
struct NodeRow {
    id: uuid::Uuid,
    realm_id: uuid::Uuid,
    content: serde_json::Value,
}

#[derive(FromRow)]
struct EdgeRow {
    source_node_id: uuid::Uuid,
    target_node_id: uuid::Uuid,
}

impl MemoryStore {
    /// Returns the nodes and `operates_on` edges scoped to a single realm.
    ///
    /// Mirrors [`MemoryStore::build_global_user_model`]'s fetch-then-merge
    /// shape, but narrows to one realm instead of merging across every realm.
    pub async fn get_subgraph(&self, realm_id: RealmId) -> Result<Subgraph, MemoryStoreError> {
        let realms = self.fetch_realms().await?;
        let nodes = self.fetch_nodes().await?;
        let edges = self.fetch_operates_on_edges().await?;

        Ok(build_subgraph(realms, nodes, edges, realm_id))
    }

    /// Returns `operates_on` edges whose endpoints span two different
    /// realms, filtered by `scope`.
    pub async fn find_operates_on_edges(
        &self,
        scope: GraphScope,
    ) -> Result<Vec<Edge>, MemoryStoreError> {
        let nodes = self.fetch_nodes().await?;
        let edges = self.fetch_operates_on_edges().await?;

        Ok(select_crossing_edges(nodes, edges, scope))
    }

    async fn fetch_realms(&self) -> Result<Vec<Realm>, MemoryStoreError> {
        Ok(
            sqlx::query_as::<_, RealmRow>("SELECT id, name FROM memory_realms")
                .fetch_all(self.pool())
                .await?
                .into_iter()
                .map(|row| Realm {
                    id: RealmId(row.id),
                    name: row.name,
                })
                .collect(),
        )
    }

    async fn fetch_nodes(&self) -> Result<Vec<UserModelNode>, MemoryStoreError> {
        Ok(
            sqlx::query_as::<_, NodeRow>("SELECT id, realm_id, content FROM memory_nodes")
                .fetch_all(self.pool())
                .await?
                .into_iter()
                .map(|row| UserModelNode {
                    id: NodeId(row.id),
                    realm_id: RealmId(row.realm_id),
                    content: row.content,
                })
                .collect(),
        )
    }

    async fn fetch_operates_on_edges(&self) -> Result<Vec<Edge>, MemoryStoreError> {
        Ok(sqlx::query_as::<_, EdgeRow>(
            "SELECT source_node_id, target_node_id FROM memory_edges WHERE edge_type = 'operates_on'",
        )
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|row| Edge {
            source_node_id: NodeId(row.source_node_id),
            target_node_id: NodeId(row.target_node_id),
            edge_type: EdgeType::OperatesOn,
        })
        .collect())
    }
}

fn build_subgraph(
    realms: Vec<Realm>,
    nodes: Vec<UserModelNode>,
    edges: Vec<Edge>,
    realm_id: RealmId,
) -> Subgraph {
    let realm = realms.into_iter().find(|realm| realm.id == realm_id);

    let nodes: Vec<UserModelNode> = nodes
        .into_iter()
        .filter(|node| node.realm_id == realm_id)
        .collect();
    let node_ids: HashSet<NodeId> = nodes.iter().map(|node| node.id).collect();

    let edges = edges
        .into_iter()
        .filter(|edge| {
            node_ids.contains(&edge.source_node_id) && node_ids.contains(&edge.target_node_id)
        })
        .collect();

    Subgraph {
        realm,
        nodes,
        edges,
    }
}

fn select_crossing_edges(
    nodes: Vec<UserModelNode>,
    edges: Vec<Edge>,
    scope: GraphScope,
) -> Vec<Edge> {
    let realm_by_node: HashMap<NodeId, RealmId> = nodes
        .into_iter()
        .map(|node| (node.id, node.realm_id))
        .collect();

    edges
        .into_iter()
        .filter(|edge| {
            let (Some(&source_realm), Some(&target_realm)) = (
                realm_by_node.get(&edge.source_node_id),
                realm_by_node.get(&edge.target_node_id),
            ) else {
                return false;
            };

            if source_realm == target_realm {
                return false;
            }

            match scope {
                GraphScope::AllRealms => true,
                GraphScope::Realm(realm_id) => source_realm == realm_id || target_realm == realm_id,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn node(id_val: u128, realm: RealmId) -> UserModelNode {
        UserModelNode {
            id: NodeId(id(id_val)),
            realm_id: realm,
            content: Value::Null,
        }
    }

    fn edge(source: u128, target: u128) -> Edge {
        Edge {
            source_node_id: NodeId(id(source)),
            target_node_id: NodeId(id(target)),
            edge_type: EdgeType::OperatesOn,
        }
    }

    #[test]
    fn get_subgraph_filters_nodes_and_edges_to_one_realm() {
        let realm_a = RealmId(id(1));
        let realm_b = RealmId(id(2));
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
        let nodes = vec![node(10, realm_a), node(11, realm_a), node(12, realm_b)];
        let edges = vec![
            edge(10, 11), // within realm_a
            edge(11, 12), // crosses realm_a -> realm_b
        ];

        let subgraph = build_subgraph(realms, nodes, edges, realm_a);

        assert_eq!(subgraph.realm.unwrap().name, "personal");
        assert_eq!(subgraph.nodes.len(), 2);
        assert!(subgraph.nodes.iter().all(|node| node.realm_id == realm_a));
        assert_eq!(subgraph.edges, vec![edge(10, 11)]);
    }

    #[test]
    fn get_subgraph_returns_no_realm_for_unknown_realm_id() {
        let subgraph = build_subgraph(vec![], vec![], vec![], RealmId(id(99)));
        assert!(subgraph.realm.is_none());
        assert!(subgraph.nodes.is_empty());
        assert!(subgraph.edges.is_empty());
    }

    #[test]
    fn find_operates_on_edges_returns_only_cross_realm_edges() {
        let realm_a = RealmId(id(1));
        let realm_b = RealmId(id(2));
        let realm_c = RealmId(id(3));
        let nodes = vec![node(10, realm_a), node(11, realm_b), node(12, realm_c)];
        let edges = vec![edge(10, 11), edge(11, 12)];

        let crossing = select_crossing_edges(nodes, edges.clone(), GraphScope::AllRealms);

        assert_eq!(crossing.len(), 2);
        assert!(crossing.contains(&edge(10, 11)));
        assert!(crossing.contains(&edge(11, 12)));
    }

    #[test]
    fn find_operates_on_edges_scoped_to_realm_excludes_unrelated_crossings() {
        let realm_a = RealmId(id(1));
        let realm_b = RealmId(id(2));
        let realm_c = RealmId(id(3));
        let nodes = vec![node(10, realm_a), node(11, realm_b), node(12, realm_c)];
        let edges = vec![edge(10, 11), edge(11, 12)];

        let crossing = select_crossing_edges(nodes, edges, GraphScope::Realm(realm_a));

        assert_eq!(crossing, vec![edge(10, 11)]);
    }

    #[test]
    fn find_operates_on_edges_excludes_same_realm_and_dangling_edges() {
        let realm_a = RealmId(id(1));
        let nodes = vec![node(10, realm_a), node(11, realm_a)];
        let edges = vec![edge(10, 11), edge(10, 99)];

        let crossing = select_crossing_edges(nodes, edges, GraphScope::AllRealms);

        assert!(crossing.is_empty());
    }
}
