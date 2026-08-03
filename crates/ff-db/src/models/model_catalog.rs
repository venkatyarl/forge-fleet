//! Typed persistence model for the model download/routing catalog.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::FromRow;

/// Exact live column order for the canonical model catalog contract.
pub const FLEET_MODEL_CATALOG_COLUMNS: &str = "id, name, family, parameters, tier, description, gated, preferred_workloads, variants, updated_at, tool_calling, display_name, tasks, modalities, benchmarks, license, lifecycle";

/// The stable, model-facing projection of a row in `fleet_model_catalog`.
///
/// `display_name`, `tasks`, `modalities`, `benchmarks`, `license`, and
/// `lifecycle` are optional: they were added by schema V243 and existing
/// rows (plus the `ff model sync-catalog` writer) predate them.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FleetModelCatalog {
    pub id: String,
    pub name: String,
    pub family: String,
    pub parameters: String,
    pub tier: i32,
    pub description: Option<String>,
    pub gated: bool,
    pub preferred_workloads: JsonValue,
    pub variants: JsonValue,
    pub tool_calling: bool,
    pub updated_at: DateTime<Utc>,
    /// Human-facing display name, distinct from the slug-like `name`. V243.
    pub display_name: Option<String>,
    /// Tasks this model supports, e.g. `["chat", "code", "reasoning"]`. V243.
    pub tasks: Option<JsonValue>,
    /// Input/output modalities, e.g. `["text", "vision"]`. V243.
    pub modalities: Option<JsonValue>,
    /// Benchmark scores keyed by suite name, e.g. `{"mmlu": 82.1}`. V243.
    pub benchmarks: Option<JsonValue>,
    /// License identifier, e.g. "apache-2.0". V243.
    pub license: Option<String>,
    /// Lifecycle status, e.g. "ga" | "preview" | "deprecated". V243.
    pub lifecycle: Option<String>,
}

impl FleetModelCatalog {
    /// Only explicitly adopted rows may participate in routing or coverage.
    pub fn is_active(&self) -> bool {
        matches!(self.lifecycle.as_deref(), Some("active" | "adopt" | "ga"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_projection_matches_exact_live_seventeen_column_schema() {
        let columns: Vec<_> = FLEET_MODEL_CATALOG_COLUMNS.split(", ").collect();
        assert_eq!(columns.len(), 17);
        assert_eq!(columns[0], "id");
        assert_eq!(columns[16], "lifecycle");
        assert!(!columns.contains(&"lifecycle_status"));
    }
}
