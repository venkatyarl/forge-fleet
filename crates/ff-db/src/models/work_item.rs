//! Typed persistence model for project-management work items.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, postgres::PgRow};
use uuid::Uuid;

/// A model that SQLx can materialize from a PostgreSQL query.
pub trait Queryable: for<'row> FromRow<'row, PgRow> {}

/// A persistent model with a stable primary key.
pub trait Identifiable {
    type Id;

    fn id(&self) -> &Self::Id;
}

/// Marker for models that may be passed to SQLx insert and update queries.
///
/// SQLx deliberately keeps persistence operations in queries rather than on
/// model types. This trait captures the bounds required by those queries while
/// retaining the active-model vocabulary used by the rest of the application.
pub trait ActiveModel: Queryable + Identifiable {}

/// The persistent representation of a row in `work_items`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct WorkItem {
    pub id: Uuid,
    pub project_id: String,
    pub milestone_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub kind: String,
    pub title: String,
    pub description: Option<String>,
    pub labels: Value,
    pub status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub assigned_computer: Option<String>,
    pub branch_name: Option<String>,
    pub pr_url: Option<String>,
    pub brain_node_ids: Value,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    pub estimated_hours: Option<f64>,
    pub metadata: Value,
    pub required_capabilities: Value,
    pub complexity: String,
    pub predicted_paths: Value,
    pub touched_paths: Value,
    pub base_branch: Option<String>,
    pub base_sha: Option<String>,
    pub integration_branch: Option<String>,
    pub merge_rank: Option<i32>,
    pub risk_score: f32,
    pub reviewer_required: bool,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub repo_id: Option<Uuid>,
    pub repo_url: Option<String>,
    pub repo_path: Option<String>,
    pub context: Value,
    pub parked: bool,
    pub pre_work: Value,
    pub work: Value,
    pub post_work: Value,
    pub cleanup_complete: bool,
    pub original_signal: Value,
    pub signal_cleared: Option<bool>,
    pub signal_verified_at: Option<DateTime<Utc>>,
    pub refiled_from: Option<Uuid>,
    pub cortex_subgraph_id: Option<String>,
}

impl Queryable for WorkItem {}

impl Identifiable for WorkItem {
    type Id = Uuid;

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl ActiveModel for WorkItem {}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_model_traits<T: Queryable + Identifiable<Id = Uuid> + ActiveModel>() {}

    #[test]
    fn work_item_supports_persistence_traits() {
        assert_model_traits::<WorkItem>();
    }
}
