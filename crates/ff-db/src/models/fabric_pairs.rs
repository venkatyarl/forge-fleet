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
