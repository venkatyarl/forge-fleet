//! Typed persistence model for the realm-scoped memory graph.
//!
//! Rows in `memory_nodes` / `memory_edges` carry an optional `realm_id`
//! column: `NULL` means the row is global (visible across every realm),
//! `Some(_)` scopes it to one `Realm`. Both `realm_id` and `edge_type` reuse
//! `ff_memory`'s canonical `RealmId` / `EdgeType` types so the two crates
//! never define competing edge-type vocabularies.

use ff_memory::{EdgeType, RealmId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The persistent representation of a row in `memory_nodes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryNode {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm_id: Option<RealmId>,
    pub content: Value,
}

/// The persistent representation of a row in `memory_edges`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEdge {
    pub source_node_id: Uuid,
    pub target_node_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm_id: Option<RealmId>,
    pub edge_type: EdgeType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_realm_id_round_trips_through_json() {
        let scoped = MemoryNode {
            id: Uuid::from_u128(1),
            realm_id: Some(RealmId(Uuid::from_u128(2))),
            content: Value::String("hello".into()),
        };
        let json = serde_json::to_value(&scoped).unwrap();
        assert_eq!(
            json["realm_id"],
            Value::String(Uuid::from_u128(2).to_string())
        );
        assert_eq!(scoped, serde_json::from_value(json).unwrap());

        let global = MemoryNode {
            id: Uuid::from_u128(1),
            realm_id: None,
            content: Value::Null,
        };
        let json = serde_json::to_value(&global).unwrap();
        assert!(json.get("realm_id").is_none());
        assert_eq!(global, serde_json::from_value(json).unwrap());
    }

    #[test]
    fn edge_type_serializes_to_operates_on_column_value() {
        let edge = MemoryEdge {
            source_node_id: Uuid::from_u128(1),
            target_node_id: Uuid::from_u128(2),
            realm_id: None,
            edge_type: EdgeType::OperatesOn,
        };
        let json = serde_json::to_value(&edge).unwrap();
        assert_eq!(json["edge_type"], Value::String("operates_on".into()));
    }
}
