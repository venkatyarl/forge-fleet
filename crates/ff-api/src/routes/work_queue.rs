//! Work queue API routes.
//!
//! Provides a lightweight, in-memory work queue for submitting work items,
//! retrieving them by ID or as the next pending item, and updating their status.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{error::ApiError, server::AppState};

/// Request body for submitting a new work item.
#[derive(Debug, Clone, Deserialize)]
pub struct SubmitWorkItemRequest {
    pub kind: String,
    pub payload: Value,
    #[serde(default)]
    pub priority: i32,
}

/// Request body for updating a work item's status.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateStatusRequest {
    pub status: String,
}

/// Work item response payload.
#[derive(Debug, Clone, Serialize)]
pub struct WorkItemResponse {
    pub id: String,
    pub kind: String,
    pub payload: Value,
    pub status: String,
    pub priority: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// In-memory work queue.
#[derive(Debug, Default)]
pub struct WorkQueue {
    items: DashMap<String, WorkQueueItem>,
}

#[derive(Debug, Clone)]
struct WorkQueueItem {
    id: String,
    kind: String,
    payload: Value,
    status: String,
    priority: i32,
    created_at: i64,
    updated_at: i64,
}

impl WorkQueue {
    /// Create an empty work queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a new work item and return its representation.
    pub fn submit(&self, req: SubmitWorkItemRequest) -> WorkItemResponse {
        let now = Utc::now().timestamp();
        let id = Uuid::new_v4().to_string();
        let item = WorkQueueItem {
            id: id.clone(),
            kind: req.kind,
            payload: req.payload,
            status: "pending".to_string(),
            priority: req.priority,
            created_at: now,
            updated_at: now,
        };
        let response = item.to_response();
        self.items.insert(id, item);
        response
    }

    /// Get a work item by ID.
    pub fn get(&self, id: &str) -> Option<WorkItemResponse> {
        self.items.get(id).map(|entry| entry.value().to_response())
    }

    /// Return the highest-priority pending work item, breaking ties by creation time.
    pub fn next_pending(&self) -> Option<WorkItemResponse> {
        self.items
            .iter()
            .filter(|entry| entry.value().status == "pending")
            .max_by_key(|entry| {
                let item = entry.value();
                (item.priority, item.created_at)
            })
            .map(|entry| entry.value().to_response())
    }

    /// Update a work item's status.
    pub fn update_status(&self, id: &str, status: &str) -> Result<WorkItemResponse, ApiError> {
        const VALID_STATUSES: &[&str] = &["pending", "running", "completed", "failed", "cancelled"];
        if !VALID_STATUSES.contains(&status) {
            return Err(ApiError::BadRequest(format!(
                "invalid status '{}'; must be one of: {}",
                status,
                VALID_STATUSES.join(", ")
            )));
        }

        let mut entry = self
            .items
            .get_mut(id)
            .ok_or_else(|| ApiError::BadRequest(format!("work item '{}' not found", id)))?;
        entry.status = status.to_string();
        entry.updated_at = Utc::now().timestamp();
        Ok(entry.to_response())
    }

    /// List all work items.
    pub fn list(&self) -> Vec<WorkItemResponse> {
        self.items
            .iter()
            .map(|entry| entry.value().to_response())
            .collect()
    }
}

impl WorkQueueItem {
    fn to_response(&self) -> WorkItemResponse {
        WorkItemResponse {
            id: self.id.clone(),
            kind: self.kind.clone(),
            payload: self.payload.clone(),
            status: self.status.clone(),
            priority: self.priority,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Submit a new work item.
pub async fn submit_work_item(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SubmitWorkItemRequest>,
) -> Result<(StatusCode, Json<WorkItemResponse>), ApiError> {
    if payload.kind.is_empty() {
        return Err(ApiError::BadRequest("kind is required".to_string()));
    }
    let item = state.work_queue.submit(payload);
    Ok((StatusCode::CREATED, Json(item)))
}

/// Get a work item by ID.
pub async fn get_work_item(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkItemResponse>, ApiError> {
    state
        .work_queue
        .get(&id)
        .map(Json)
        .ok_or_else(|| ApiError::BadRequest(format!("work item '{}' not found", id)))
}

/// Retrieve the next pending work item.
pub async fn get_next_work_item(
    State(state): State<Arc<AppState>>,
) -> Result<Json<WorkItemResponse>, ApiError> {
    state
        .work_queue
        .next_pending()
        .map(Json)
        .ok_or_else(|| ApiError::BadRequest("no pending work items".to_string()))
}

/// Update the status of a work item.
pub async fn update_work_item_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateStatusRequest>,
) -> Result<Json<WorkItemResponse>, ApiError> {
    if payload.status.is_empty() {
        return Err(ApiError::BadRequest("status is required".to_string()));
    }
    state
        .work_queue
        .update_status(&id, &payload.status)
        .map(Json)
}

/// List all work items.
pub async fn list_work_items(State(state): State<Arc<AppState>>) -> Json<Vec<WorkItemResponse>> {
    Json(state.work_queue.list())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_and_get_round_trip() {
        let queue = WorkQueue::new();
        let item = queue.submit(SubmitWorkItemRequest {
            kind: "test".to_string(),
            payload: serde_json::json!({"foo": "bar"}),
            priority: 0,
        });

        assert_eq!(item.kind, "test");
        assert_eq!(item.status, "pending");

        let fetched = queue.get(&item.id).unwrap();
        assert_eq!(fetched.id, item.id);
        assert_eq!(fetched.payload, serde_json::json!({"foo": "bar"}));
    }

    #[test]
    fn next_pending_selects_highest_priority() {
        let queue = WorkQueue::new();
        let low = queue.submit(SubmitWorkItemRequest {
            kind: "low".to_string(),
            payload: Value::Null,
            priority: 1,
        });
        let high = queue.submit(SubmitWorkItemRequest {
            kind: "high".to_string(),
            payload: Value::Null,
            priority: 10,
        });

        let next = queue.next_pending().unwrap();
        assert_eq!(next.id, high.id);
        assert_ne!(next.id, low.id);
    }

    #[test]
    fn update_status_changes_state() {
        let queue = WorkQueue::new();
        let item = queue.submit(SubmitWorkItemRequest {
            kind: "test".to_string(),
            payload: Value::Null,
            priority: 0,
        });

        let updated = queue.update_status(&item.id, "running").unwrap();
        assert_eq!(updated.status, "running");

        let fetched = queue.get(&item.id).unwrap();
        assert_eq!(fetched.status, "running");
        assert!(fetched.updated_at >= updated.created_at);
    }

    #[test]
    fn update_invalid_status_returns_error() {
        let queue = WorkQueue::new();
        let item = queue.submit(SubmitWorkItemRequest {
            kind: "test".to_string(),
            payload: Value::Null,
            priority: 0,
        });

        let result = queue.update_status(&item.id, "bogus");
        assert!(result.is_err());
    }

    #[test]
    fn completed_items_are_not_next_pending() {
        let queue = WorkQueue::new();
        let item = queue.submit(SubmitWorkItemRequest {
            kind: "test".to_string(),
            payload: Value::Null,
            priority: 0,
        });

        queue.update_status(&item.id, "completed").unwrap();
        assert!(queue.next_pending().is_none());
    }

    #[test]
    fn list_returns_all_items() {
        let queue = WorkQueue::new();
        queue.submit(SubmitWorkItemRequest {
            kind: "a".to_string(),
            payload: Value::Null,
            priority: 0,
        });
        queue.submit(SubmitWorkItemRequest {
            kind: "b".to_string(),
            payload: Value::Null,
            priority: 0,
        });

        assert_eq!(queue.list().len(), 2);
    }
}
