//! Typed persistence model for fabric topology pairs.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// The stable, model-facing projection of a row in `fabric_pairs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct FabricPair {
    pub id: Uuid,
    pub source_node: String,
    pub target_node: String,
    pub cidr: String,
    pub status: String,
    pub verified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn represents_ring_one_pairs() {
        for (source_node, target_node, cidr) in [
            ("shakira", "rihanna", "10.42.0.0/30"),
            ("rihanna", "beyonce", "10.43.0.0/30"),
        ] {
            let pair = FabricPair {
                id: Uuid::nil(),
                source_node: source_node.into(),
                target_node: target_node.into(),
                cidr: cidr.into(),
                status: "pending".into(),
                verified: false,
            };

            assert_eq!(pair.source_node, source_node);
            assert_eq!(pair.target_node, target_node);
        }
    }
}
