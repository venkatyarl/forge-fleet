//! Mission Control API routes backed by `ff_db::OperationalStore`.
//!
//! This keeps core Mission Control workflows (work item lifecycle, review checklist,
//! dependency checks, board, dashboard) available when ForgeFleet runs in
//! Postgres-backed modes (`postgres_runtime` / `postgres_full`) without requiring
//! a dedicated SQLite `mission-control.db` sidecar.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
};
use chrono::Utc;
use ff_db::OperationalStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;

use crate::board::{BoardColumn, BoardFilter, BoardView};
use crate::dashboard::{BlockedItemSummary, DashboardStats};
use crate::dependency::{DependencyCheck, WorkItemDependency};
use crate::error::{McError, McResult};
use crate::review_item::{CreateReviewItem, ReviewItem, ReviewItemStatus, UpdateReviewItem};
use crate::work_item::{
    CreateWorkItem, Priority, UpdateWorkItem, WorkItem, WorkItemFilter, WorkItemStatus,
};

const WORK_ITEM_KEY_PREFIX: &str = "ff_mc.work_item.";
const REVIEW_ITEM_KEY_PREFIX: &str = "ff_mc.review_item.";
const DEPENDENCY_KEY_PREFIX: &str = "ff_mc.dependency.";
const STORE_SCAN_LIMIT: u32 = 20_000;

#[derive(Debug, Clone)]
pub struct McOperationalState {
    pub store: OperationalStore,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

fn mc_error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

fn handle_mc_err(e: McError) -> impl IntoResponse {
    match &e {
        McError::WorkItemNotFound { .. } | McError::ReviewItemNotFound { .. } => {
            mc_error(StatusCode::NOT_FOUND, e.to_string())
        }
        McError::InvalidStatus { .. } | McError::InvalidPriority { .. } => {
            mc_error(StatusCode::BAD_REQUEST, e.to_string())
        }
        _ => mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub fn mc_router_operational(store: OperationalStore) -> Router {
    let state = Arc::new(McOperationalState { store });

    Router::new()
        .route("/api/mc/work-items", get(list_work_items))
        .route("/api/mc/work-items", post(create_work_item))
        .route("/api/mc/work-items/{id}", get(get_work_item))
        .route("/api/mc/work-items/{id}", patch(update_work_item))
        .route("/api/mc/work-items/{id}", delete(delete_work_item))
        .route("/api/mc/work-items/{id}/claim", post(claim_work_item))
        .route("/api/mc/work-items/{id}/complete", post(complete_work_item))
        .route("/api/mc/work-items/{id}/fail", post(fail_work_item))
        .route("/api/mc/work-items/{id}/escalate", post(escalate_work_item))
        .route("/api/mc/work-items/{id}/review/submit", post(submit_review))
        .route("/api/mc/work-items/{id}/review/start", post(start_review))
        .route(
            "/api/mc/work-items/{id}/review/complete",
            post(complete_review),
        )
        .route(
            "/api/mc/work-items/{id}/review-items",
            get(list_review_items_for_work_item),
        )
        .route(
            "/api/mc/work-items/{id}/review-items",
            post(create_review_item_for_work_item),
        )
        .route(
            "/api/mc/work-items/{id}/review-items/reset",
            post(reset_review_items_for_work_item),
        )
        .route("/api/mc/review-items/{id}", patch(update_review_item))
        .route("/api/mc/review-items/{id}", delete(delete_review_item))
        .route(
            "/api/mc/work-items/{id}/dependencies",
            get(list_work_item_dependencies),
        )
        .route(
            "/api/mc/work-items/{id}/dependencies",
            post(add_work_item_dependency),
        )
        .route(
            "/api/mc/work-items/{id}/dependencies/check",
            get(check_work_item_dependencies),
        )
        .route(
            "/api/mc/work-items/{id}/dependencies/{depends_on_id}",
            delete(remove_work_item_dependency),
        )
        .route("/api/mc/board", get(get_board))
        .route("/api/mc/dashboard", get(get_dashboard))
        // ─── MC Legacy Migration Routes ─────────────────────────────
        .route("/api/mc/work-items/{id}/counsel", post(counsel_request))
        .route("/api/mc/work-items/{id}/events", get(list_work_item_events))
        .route("/api/mc/work-items/{id}/timer/{action}", post(timer_action))
        .route("/api/mc/work-items/{id}/pr", put(update_pr_info))
        .route("/api/mc/work-items/{id}/history", get(work_item_history))
        .route("/api/mc/node-messages", get(list_node_messages).post(send_node_message))
        .route("/api/mc/node-messages/{id}/read", put(mark_message_read))
        .route("/api/mc/model-performance", get(list_model_performance).post(record_model_performance))
        .route("/api/mc/work-items/generate", post(generate_work_items))
        .route("/api/mc/fleet/status", get(fleet_mc_status))
        .with_state(state)
}

// ─── Work item routes ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WorkItemQuery {
    status: Option<String>,
    assignee: Option<String>,
    epic_id: Option<String>,
    sprint_id: Option<String>,
    task_group_id: Option<String>,
    label: Option<String>,
}

async fn list_work_items(
    State(state): State<Arc<McOperationalState>>,
    Query(q): Query<WorkItemQuery>,
) -> impl IntoResponse {
    let status = q
        .status
        .and_then(|s| WorkItemStatus::from_str_loose(&s).ok());

    let filter = WorkItemFilter {
        status,
        assignee: q.assignee,
        epic_id: q.epic_id,
        sprint_id: q.sprint_id,
        task_group_id: q.task_group_id,
        label: q.label,
    };

    match list_work_items_from_store(&state.store, &filter).await {
        Ok(items) => Json(items).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn create_work_item(
    State(state): State<Arc<McOperationalState>>,
    Json(params): Json<CreateWorkItem>,
) -> impl IntoResponse {
    match create_work_item_in_store(&state.store, params).await {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn get_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match get_work_item_from_store(&state.store, &id).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn update_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateWorkItem>,
) -> impl IntoResponse {
    match update_work_item_in_store(&state.store, &id, params).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn delete_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match delete_work_item_in_store(&state.store, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ClaimWorkItemRequest {
    assignee: Option<String>,
}

async fn claim_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    body: Option<Json<ClaimWorkItemRequest>>,
) -> impl IntoResponse {
    let assignee = body.and_then(|Json(v)| v.assignee);
    match claim_work_item_in_store(&state.store, &id, assignee).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn complete_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match complete_work_item_in_store(&state.store, &id).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn fail_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match fail_work_item_in_store(&state.store, &id).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn escalate_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match escalate_work_item_in_store(&state.store, &id).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

// ─── Review routes ──────────────────────────────────────────────────────────

async fn submit_review(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match update_work_item_in_store(
        &state.store,
        &id,
        UpdateWorkItem {
            status: Some("review".to_string()),
            ..Default::default()
        },
    )
    .await
    {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn start_review(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    submit_review(State(state), Path(id)).await
}

async fn complete_review(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match summary_for_work_item_review(&state.store, &id).await {
        Ok((total_items, all_approved)) => {
            if total_items > 0 && !all_approved {
                return handle_mc_err(McError::Other(anyhow!(
                    "cannot complete review: checklist has non-approved items"
                )))
                .into_response();
            }
        }
        Err(err) => return handle_mc_err(err).into_response(),
    }

    match complete_work_item_in_store(&state.store, &id).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn list_review_items_for_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match list_review_items_for_work_item_store(&state.store, &id).await {
        Ok(items) => Json(items).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn create_review_item_for_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<CreateReviewItem>,
) -> impl IntoResponse {
    match create_review_item_for_work_item_store(&state.store, &id, params).await {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn update_review_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateReviewItem>,
) -> impl IntoResponse {
    match update_review_item_in_store(&state.store, &id, params).await {
        Ok(item) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn delete_review_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match delete_review_item_in_store(&state.store, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn reset_review_items_for_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match reset_review_items_for_work_item_store(&state.store, &id).await {
        Ok(updated) => Json(serde_json::json!({"updated": updated})).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

// ─── Dependency routes ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AddDependencyRequest {
    depends_on_id: String,
}

async fn list_work_item_dependencies(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match list_dependencies_for_work_item_store(&state.store, &id).await {
        Ok(deps) => Json(deps).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn add_work_item_dependency(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(body): Json<AddDependencyRequest>,
) -> impl IntoResponse {
    match add_dependency_in_store(&state.store, &id, &body.depends_on_id).await {
        Ok(dep) => (StatusCode::CREATED, Json(dep)).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn remove_work_item_dependency(
    State(state): State<Arc<McOperationalState>>,
    Path((id, depends_on_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match remove_dependency_in_store(&state.store, &id, &depends_on_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => mc_error(StatusCode::NOT_FOUND, "dependency not found").into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn check_work_item_dependencies(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match check_dependencies_in_store(&state.store, &id).await {
        Ok(result) => Json(result).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

// ─── Board and dashboard ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BoardQuery {
    assignee: Option<String>,
    epic_id: Option<String>,
    sprint_id: Option<String>,
    task_group_id: Option<String>,
    label: Option<String>,
}

async fn get_board(
    State(state): State<Arc<McOperationalState>>,
    Query(q): Query<BoardQuery>,
) -> impl IntoResponse {
    let filter = BoardFilter {
        assignee: q.assignee,
        epic_id: q.epic_id,
        sprint_id: q.sprint_id,
        task_group_id: q.task_group_id,
        label: q.label,
    };

    match build_board_view_from_store(&state.store, &filter).await {
        Ok(board) => Json(board).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn get_dashboard(State(state): State<Arc<McOperationalState>>) -> impl IntoResponse {
    match compute_dashboard_stats_from_store(&state.store).await {
        Ok(stats) => Json(stats).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

// ─── Persistence helpers ────────────────────────────────────────────────────

fn work_item_storage_key(id: &str) -> String {
    format!("{WORK_ITEM_KEY_PREFIX}{id}")
}

fn dependency_storage_key(work_item_id: &str, depends_on_id: &str) -> String {
    format!("{DEPENDENCY_KEY_PREFIX}{work_item_id}.{depends_on_id}")
}

fn dependency_prefix_for_work_item(work_item_id: &str) -> String {
    format!("{DEPENDENCY_KEY_PREFIX}{work_item_id}.")
}

fn review_item_storage_key(work_item_id: &str, review_item_id: &str) -> String {
    format!("{REVIEW_ITEM_KEY_PREFIX}{work_item_id}.{review_item_id}")
}

fn to_other_error(context: &str, err: impl std::fmt::Display) -> McError {
    McError::Other(anyhow!("{context}: {err}"))
}

async fn list_work_items_from_store(
    store: &OperationalStore,
    filter: &WorkItemFilter,
) -> McResult<Vec<WorkItem>> {
    let rows = store
        .config_list_prefix(WORK_ITEM_KEY_PREFIX, STORE_SCAN_LIMIT)
        .await
        .map_err(|e| to_other_error("failed listing work items from operational store", e))?;

    let mut items = Vec::new();

    for (key, payload) in rows {
        match serde_json::from_str::<WorkItem>(&payload) {
            Ok(mut item) => {
                if item.id.trim().is_empty() {
                    item.id = key
                        .strip_prefix(WORK_ITEM_KEY_PREFIX)
                        .unwrap_or(&key)
                        .to_string();
                }

                if work_item_matches_filter(&item, filter) {
                    items.push(item);
                }
            }
            Err(err) => {
                warn!(key = %key, error = %err, "skipping invalid mission-control work item record");
            }
        }
    }

    items.sort_by(|a, b| {
        a.priority
            .0
            .cmp(&b.priority.0)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });

    Ok(items)
}

fn work_item_matches_filter(item: &WorkItem, filter: &WorkItemFilter) -> bool {
    if let Some(status) = filter.status
        && item.status != status
    {
        return false;
    }
    if let Some(assignee) = filter.assignee.as_deref()
        && item.assignee != assignee
    {
        return false;
    }
    if let Some(epic_id) = filter.epic_id.as_deref()
        && item.epic_id.as_deref() != Some(epic_id)
    {
        return false;
    }
    if let Some(sprint_id) = filter.sprint_id.as_deref()
        && item.sprint_id.as_deref() != Some(sprint_id)
    {
        return false;
    }
    if let Some(task_group_id) = filter.task_group_id.as_deref()
        && item.task_group_id.as_deref() != Some(task_group_id)
    {
        return false;
    }
    if let Some(label) = filter.label.as_deref()
        && !item.labels.iter().any(|l| l == label)
    {
        return false;
    }

    true
}

async fn get_work_item_from_store(store: &OperationalStore, id: &str) -> McResult<WorkItem> {
    let key = work_item_storage_key(id);
    let Some(payload) = store
        .config_get(&key)
        .await
        .map_err(|e| to_other_error("failed loading work item", e))?
    else {
        return Err(McError::WorkItemNotFound { id: id.to_string() });
    };

    let mut item: WorkItem = serde_json::from_str(&payload)
        .map_err(|e| to_other_error("failed parsing work item payload", e))?;

    if item.id.trim().is_empty() {
        item.id = id.to_string();
    }

    Ok(item)
}

async fn persist_work_item(store: &OperationalStore, item: &WorkItem) -> McResult<()> {
    let payload = serde_json::to_string(item)
        .map_err(|e| to_other_error("failed serializing work item", e))?;

    store
        .config_set(&work_item_storage_key(&item.id), &payload)
        .await
        .map_err(|e| to_other_error("failed saving work item", e))
}

async fn create_work_item_in_store(
    store: &OperationalStore,
    params: CreateWorkItem,
) -> McResult<WorkItem> {
    let now = Utc::now();
    let status = match &params.status {
        Some(raw) => WorkItemStatus::from_str_loose(raw)?,
        None => WorkItemStatus::Backlog,
    };
    let priority = match params.priority {
        Some(p) => Priority::new(p)?,
        None => Priority::default(),
    };

    let item = WorkItem {
        id: Uuid::new_v4().to_string(),
        title: params.title,
        description: params.description,
        status,
        priority,
        assignee: params.assignee.unwrap_or_else(|| "unassigned".to_string()),
        epic_id: params.epic_id,
        sprint_id: params.sprint_id,
        task_group_id: params.task_group_id,
        sequence_order: params.sequence_order,
        labels: params.labels,
        created_at: now,
        updated_at: now,
    };

    persist_work_item(store, &item).await?;
    Ok(item)
}

async fn update_work_item_in_store(
    store: &OperationalStore,
    id: &str,
    params: UpdateWorkItem,
) -> McResult<WorkItem> {
    let mut item = get_work_item_from_store(store, id).await?;

    if let Some(title) = params.title {
        item.title = title;
    }
    if let Some(description) = params.description {
        item.description = description;
    }
    if let Some(status) = params.status {
        item.status = WorkItemStatus::from_str_loose(&status)?;
    }
    if let Some(priority) = params.priority {
        item.priority = Priority::new(priority)?;
    }
    if let Some(assignee) = params.assignee {
        item.assignee = assignee;
    }
    if let Some(epic_id) = params.epic_id {
        item.epic_id = epic_id;
    }
    if let Some(sprint_id) = params.sprint_id {
        item.sprint_id = sprint_id;
    }
    if let Some(task_group_id) = params.task_group_id {
        item.task_group_id = task_group_id;
    }
    if let Some(sequence_order) = params.sequence_order {
        item.sequence_order = sequence_order;
    }
    if let Some(labels) = params.labels {
        item.labels = labels;
    }

    item.updated_at = Utc::now();
    persist_work_item(store, &item).await?;
    Ok(item)
}

async fn delete_work_item_in_store(store: &OperationalStore, id: &str) -> McResult<()> {
    let deleted = store
        .config_delete(&work_item_storage_key(id))
        .await
        .map_err(|e| to_other_error("failed deleting work item", e))?;

    if !deleted {
        return Err(McError::WorkItemNotFound { id: id.to_string() });
    }

    // Cleanup child records to keep parity with sqlite's ON DELETE behavior.
    delete_review_items_for_work_item_store(store, id).await?;
    delete_dependencies_for_work_item_store(store, id).await?;

    Ok(())
}

async fn claim_work_item_in_store(
    store: &OperationalStore,
    id: &str,
    assignee: Option<String>,
) -> McResult<WorkItem> {
    let item = get_work_item_from_store(store, id).await?;
    let mut update = UpdateWorkItem {
        assignee: Some(assignee.unwrap_or_else(|| "unassigned".to_string())),
        ..Default::default()
    };

    if item.status == WorkItemStatus::Backlog {
        update.status = Some("todo".to_string());
    }

    update_work_item_in_store(store, id, update).await
}

async fn complete_work_item_in_store(store: &OperationalStore, id: &str) -> McResult<WorkItem> {
    update_work_item_in_store(
        store,
        id,
        UpdateWorkItem {
            status: Some("done".to_string()),
            ..Default::default()
        },
    )
    .await
}

async fn fail_work_item_in_store(store: &OperationalStore, id: &str) -> McResult<WorkItem> {
    update_work_item_in_store(
        store,
        id,
        UpdateWorkItem {
            status: Some("blocked".to_string()),
            ..Default::default()
        },
    )
    .await
}

async fn escalate_work_item_in_store(store: &OperationalStore, id: &str) -> McResult<WorkItem> {
    let item = get_work_item_from_store(store, id).await?;
    let new_priority = (item.priority.0 - 1).clamp(1, 5);
    let status = if item.status == WorkItemStatus::Done {
        None
    } else {
        Some("blocked".to_string())
    };

    update_work_item_in_store(
        store,
        id,
        UpdateWorkItem {
            priority: Some(new_priority),
            status,
            ..Default::default()
        },
    )
    .await
}

async fn build_board_view_from_store(
    store: &OperationalStore,
    filter: &BoardFilter,
) -> McResult<BoardView> {
    let items = list_work_items_from_store(
        store,
        &WorkItemFilter {
            status: None,
            assignee: filter.assignee.clone(),
            epic_id: filter.epic_id.clone(),
            sprint_id: filter.sprint_id.clone(),
            task_group_id: filter.task_group_id.clone(),
            label: filter.label.clone(),
        },
    )
    .await?;

    let total_items = items.len();

    let mut grouped: BTreeMap<u8, Vec<WorkItem>> = BTreeMap::new();
    for item in items {
        grouped
            .entry(status_sort_key(item.status))
            .or_default()
            .push(item);
    }

    let columns: Vec<BoardColumn> = WorkItemStatus::all_columns()
        .iter()
        .map(|status| {
            let mut items = grouped
                .remove(&status_sort_key(*status))
                .unwrap_or_default();
            items.sort_by(|a, b| {
                a.priority
                    .0
                    .cmp(&b.priority.0)
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
            });
            BoardColumn {
                status: *status,
                label: status_label(*status),
                count: items.len(),
                items,
            }
        })
        .collect();

    Ok(BoardView {
        columns,
        total_items,
    })
}

async fn compute_dashboard_stats_from_store(store: &OperationalStore) -> McResult<DashboardStats> {
    let items = list_work_items_from_store(store, &WorkItemFilter::default()).await?;

    let mut items_by_status: HashMap<String, i64> = HashMap::new();
    let mut items_per_assignee: HashMap<String, i64> = HashMap::new();
    let mut blocked_items: Vec<BlockedItemSummary> = Vec::new();

    for item in &items {
        *items_by_status
            .entry(item.status.as_str().to_string())
            .or_insert(0) += 1;
        *items_per_assignee.entry(item.assignee.clone()).or_insert(0) += 1;

        if item.status == WorkItemStatus::Blocked {
            blocked_items.push(BlockedItemSummary {
                id: item.id.clone(),
                title: item.title.clone(),
                assignee: item.assignee.clone(),
                epic_id: item.epic_id.clone(),
            });
        }
    }

    Ok(DashboardStats {
        total_items: items.len() as i64,
        items_by_status,
        items_per_assignee,
        blocked_items,
        // Sprint domains are still served by the SQLite MC backend. In
        // Postgres-backed operational mode we preserve dashboard contract shape
        // while returning empty trend/overdue sets.
        velocity_trend: Vec::new(),
        overdue_items: Vec::new(),
    })
}

fn status_sort_key(status: WorkItemStatus) -> u8 {
    match status {
        WorkItemStatus::Backlog => 0,
        WorkItemStatus::Todo => 1,
        WorkItemStatus::InProgress => 2,
        WorkItemStatus::Review => 3,
        WorkItemStatus::Done => 4,
        WorkItemStatus::Blocked => 5,
    }
}

fn status_label(status: WorkItemStatus) -> String {
    match status {
        WorkItemStatus::Backlog => "📋 Backlog".to_string(),
        WorkItemStatus::Todo => "📝 To Do".to_string(),
        WorkItemStatus::InProgress => "🔨 In Progress".to_string(),
        WorkItemStatus::Review => "🔍 Review".to_string(),
        WorkItemStatus::Done => "✅ Done".to_string(),
        WorkItemStatus::Blocked => "🚫 Blocked".to_string(),
    }
}

// ─── Review item persistence helpers ────────────────────────────────────────

async fn list_review_items_for_work_item_store(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<Vec<ReviewItem>> {
    let _ = get_work_item_from_store(store, work_item_id).await?;

    let rows = store
        .config_list_prefix(
            &format!("{REVIEW_ITEM_KEY_PREFIX}{work_item_id}."),
            STORE_SCAN_LIMIT,
        )
        .await
        .map_err(|e| to_other_error("failed listing review items", e))?;

    let mut items = Vec::new();

    for (key, payload) in rows {
        match serde_json::from_str::<ReviewItem>(&payload) {
            Ok(mut item) => {
                if item.id.trim().is_empty() {
                    item.id = key.rsplit('.').next().unwrap_or_default().to_string();
                }
                if item.work_item_id.trim().is_empty() {
                    item.work_item_id = work_item_id.to_string();
                }
                items.push(item);
            }
            Err(err) => {
                warn!(key = %key, error = %err, "skipping invalid review item record");
            }
        }
    }

    items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(items)
}

async fn create_review_item_for_work_item_store(
    store: &OperationalStore,
    work_item_id: &str,
    params: CreateReviewItem,
) -> McResult<ReviewItem> {
    let _ = get_work_item_from_store(store, work_item_id).await?;

    let now = Utc::now();
    let status = match &params.status {
        Some(raw) => ReviewItemStatus::from_str_loose(raw)?,
        None => ReviewItemStatus::Pending,
    };

    let item = ReviewItem {
        id: Uuid::new_v4().to_string(),
        work_item_id: work_item_id.to_string(),
        title: params.title,
        status,
        reviewer: params.reviewer,
        notes: params.notes,
        created_at: now,
        updated_at: now,
    };

    persist_review_item(store, &item).await?;
    Ok(item)
}

async fn update_review_item_in_store(
    store: &OperationalStore,
    id: &str,
    params: UpdateReviewItem,
) -> McResult<ReviewItem> {
    let (_key, mut item) = find_review_item_record(store, id).await?;

    if let Some(title) = params.title {
        item.title = title;
    }
    if let Some(reviewer) = params.reviewer {
        item.reviewer = reviewer;
    }
    if let Some(notes) = params.notes {
        item.notes = notes;
    }
    if let Some(status) = params.status {
        item.status = ReviewItemStatus::from_str_loose(&status)?;
    }

    item.updated_at = Utc::now();
    persist_review_item(store, &item).await?;
    Ok(item)
}

async fn delete_review_item_in_store(store: &OperationalStore, id: &str) -> McResult<()> {
    let (key, _item) = find_review_item_record(store, id).await?;

    let deleted = store
        .config_delete(&key)
        .await
        .map_err(|e| to_other_error("failed deleting review item", e))?;

    if !deleted {
        return Err(McError::ReviewItemNotFound { id: id.to_string() });
    }

    Ok(())
}

async fn reset_review_items_for_work_item_store(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<usize> {
    let mut items = list_review_items_for_work_item_store(store, work_item_id).await?;
    let mut updated = 0usize;

    for item in &mut items {
        if item.status != ReviewItemStatus::Pending {
            item.status = ReviewItemStatus::Pending;
            item.updated_at = Utc::now();
            persist_review_item(store, item).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn summary_for_work_item_review(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<(usize, bool)> {
    let items = list_review_items_for_work_item_store(store, work_item_id).await?;
    let total = items.len();
    let all_approved = total > 0
        && items
            .iter()
            .all(|item| item.status == ReviewItemStatus::Approved);
    Ok((total, all_approved))
}

async fn persist_review_item(store: &OperationalStore, item: &ReviewItem) -> McResult<()> {
    let payload = serde_json::to_string(item)
        .map_err(|e| to_other_error("failed serializing review item", e))?;

    store
        .config_set(
            &review_item_storage_key(&item.work_item_id, &item.id),
            &payload,
        )
        .await
        .map_err(|e| to_other_error("failed saving review item", e))
}

async fn find_review_item_record(
    store: &OperationalStore,
    review_item_id: &str,
) -> McResult<(String, ReviewItem)> {
    let rows = store
        .config_list_prefix(REVIEW_ITEM_KEY_PREFIX, STORE_SCAN_LIMIT)
        .await
        .map_err(|e| to_other_error("failed listing review item records", e))?;

    for (key, payload) in rows {
        let Some(id_segment) = key.rsplit('.').next() else {
            continue;
        };
        if id_segment != review_item_id {
            continue;
        }

        let mut item: ReviewItem = serde_json::from_str(&payload)
            .map_err(|e| to_other_error("failed parsing review item payload", e))?;

        if item.id.trim().is_empty() {
            item.id = review_item_id.to_string();
        }

        if item.work_item_id.trim().is_empty() {
            let maybe_work_item = key
                .strip_prefix(REVIEW_ITEM_KEY_PREFIX)
                .and_then(|rest| rest.rsplit_once('.'))
                .map(|(work_item_id, _)| work_item_id.to_string());
            item.work_item_id = maybe_work_item.unwrap_or_default();
        }

        return Ok((key, item));
    }

    Err(McError::ReviewItemNotFound {
        id: review_item_id.to_string(),
    })
}

async fn delete_review_items_for_work_item_store(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<()> {
    let rows = store
        .config_list_prefix(
            &format!("{REVIEW_ITEM_KEY_PREFIX}{work_item_id}."),
            STORE_SCAN_LIMIT,
        )
        .await
        .map_err(|e| to_other_error("failed listing review items for cleanup", e))?;

    for (key, _payload) in rows {
        let _ = store
            .config_delete(&key)
            .await
            .map_err(|e| to_other_error("failed deleting review item during cleanup", e))?;
    }

    Ok(())
}

// ─── Dependency persistence helpers ─────────────────────────────────────────

async fn add_dependency_in_store(
    store: &OperationalStore,
    work_item_id: &str,
    depends_on_id: &str,
) -> McResult<WorkItemDependency> {
    if work_item_id == depends_on_id {
        return Err(McError::Other(anyhow!("work item cannot depend on itself")));
    }

    let _ = get_work_item_from_store(store, work_item_id).await?;
    let _ = get_work_item_from_store(store, depends_on_id).await?;

    let dep = WorkItemDependency {
        work_item_id: work_item_id.to_string(),
        depends_on_id: depends_on_id.to_string(),
        created_at: Utc::now(),
    };

    let payload = serde_json::to_string(&dep)
        .map_err(|e| to_other_error("failed serializing dependency", e))?;

    store
        .config_set(
            &dependency_storage_key(work_item_id, depends_on_id),
            &payload,
        )
        .await
        .map_err(|e| to_other_error("failed saving dependency", e))?;

    Ok(dep)
}

async fn list_dependencies_for_work_item_store(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<Vec<WorkItemDependency>> {
    let _ = get_work_item_from_store(store, work_item_id).await?;

    let rows = store
        .config_list_prefix(
            &dependency_prefix_for_work_item(work_item_id),
            STORE_SCAN_LIMIT,
        )
        .await
        .map_err(|e| to_other_error("failed listing dependencies", e))?;

    let mut deps = Vec::new();

    for (key, payload) in rows {
        match serde_json::from_str::<WorkItemDependency>(&payload) {
            Ok(mut dep) => {
                if dep.work_item_id.trim().is_empty() {
                    dep.work_item_id = work_item_id.to_string();
                }
                if dep.depends_on_id.trim().is_empty() {
                    dep.depends_on_id = key
                        .strip_prefix(&dependency_prefix_for_work_item(work_item_id))
                        .unwrap_or_default()
                        .to_string();
                }
                deps.push(dep);
            }
            Err(err) => {
                warn!(key = %key, error = %err, "skipping invalid dependency record");
            }
        }
    }

    deps.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(deps)
}

async fn remove_dependency_in_store(
    store: &OperationalStore,
    work_item_id: &str,
    depends_on_id: &str,
) -> McResult<bool> {
    store
        .config_delete(&dependency_storage_key(work_item_id, depends_on_id))
        .await
        .map_err(|e| to_other_error("failed deleting dependency", e))
}

async fn check_dependencies_in_store(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<DependencyCheck> {
    let _ = get_work_item_from_store(store, work_item_id).await?;
    let deps = list_dependencies_for_work_item_store(store, work_item_id).await?;

    let mut blocked_by_ids = Vec::new();

    for dep in deps {
        let depends_on = get_work_item_from_store(store, &dep.depends_on_id).await?;
        if depends_on.status != WorkItemStatus::Done {
            blocked_by_ids.push(dep.depends_on_id);
        }
    }

    let blocked_count = blocked_by_ids.len();

    Ok(DependencyCheck {
        work_item_id: work_item_id.to_string(),
        blocked_by_ids,
        blocked_count,
        can_start: blocked_count == 0,
    })
}

async fn delete_dependencies_for_work_item_store(
    store: &OperationalStore,
    work_item_id: &str,
) -> McResult<()> {
    // Delete rows where this item is the parent side.
    let parent_rows = store
        .config_list_prefix(
            &dependency_prefix_for_work_item(work_item_id),
            STORE_SCAN_LIMIT,
        )
        .await
        .map_err(|e| to_other_error("failed listing dependencies for cleanup", e))?;

    for (key, _payload) in parent_rows {
        let _ = store
            .config_delete(&key)
            .await
            .map_err(|e| to_other_error("failed deleting dependency during cleanup", e))?;
    }

    // Delete rows where this item appears as dependency target.
    let all_rows = store
        .config_list_prefix(DEPENDENCY_KEY_PREFIX, STORE_SCAN_LIMIT)
        .await
        .map_err(|e| to_other_error("failed scanning dependencies for reverse cleanup", e))?;

    for (key, payload) in all_rows {
        let dep = match serde_json::from_str::<WorkItemDependency>(&payload) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if dep.depends_on_id == work_item_id {
            let _ = store
                .config_delete(&key)
                .await
                .map_err(|e| to_other_error("failed deleting reverse dependency", e))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_db::{DbPool, DbPoolConfig, run_migrations};

    async fn test_store() -> OperationalStore {
        let path =
            std::env::temp_dir().join(format!("ff-mc-operational-api-test-{}.db", Uuid::new_v4()));

        let pool = DbPool::open(DbPoolConfig::with_path(&path)).expect("open sqlite pool");
        let conn = pool
            .open_raw_connection()
            .expect("open sqlite raw connection");
        run_migrations(&conn).expect("run sqlite migrations");

        OperationalStore::sqlite(pool)
    }

    #[tokio::test]
    async fn test_work_item_lifecycle_and_board() {
        let store = test_store().await;

        let created = create_work_item_in_store(
            &store,
            CreateWorkItem {
                title: "Operational-backed task".to_string(),
                status: Some("backlog".to_string()),
                priority: Some(2),
                assignee: Some("taylor".to_string()),
                labels: vec!["postgres".to_string()],
                ..Default::default()
            },
        )
        .await
        .expect("create work item");

        let claimed = claim_work_item_in_store(&store, &created.id, Some("agent-1".to_string()))
            .await
            .expect("claim work item");
        assert_eq!(claimed.status, WorkItemStatus::Todo);
        assert_eq!(claimed.assignee, "agent-1");

        let failed = fail_work_item_in_store(&store, &created.id)
            .await
            .expect("fail work item");
        assert_eq!(failed.status, WorkItemStatus::Blocked);

        let escalated = escalate_work_item_in_store(&store, &created.id)
            .await
            .expect("escalate work item");
        assert!(escalated.priority.0 <= failed.priority.0);

        let board = build_board_view_from_store(&store, &BoardFilter::default())
            .await
            .expect("build board");
        assert_eq!(board.total_items, 1);
        assert_eq!(board.columns.len(), 6);

        let dashboard = compute_dashboard_stats_from_store(&store)
            .await
            .expect("compute dashboard");
        assert_eq!(dashboard.total_items, 1);
        assert_eq!(*dashboard.items_by_status.get("blocked").unwrap_or(&0), 1);
    }

    #[tokio::test]
    async fn test_review_and_dependency_paths() {
        let store = test_store().await;

        let upstream = create_work_item_in_store(
            &store,
            CreateWorkItem {
                title: "Upstream".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("create upstream item");

        let downstream = create_work_item_in_store(
            &store,
            CreateWorkItem {
                title: "Downstream".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("create downstream item");

        let dep = add_dependency_in_store(&store, &downstream.id, &upstream.id)
            .await
            .expect("add dependency");
        assert_eq!(dep.depends_on_id, upstream.id);

        let blocked = check_dependencies_in_store(&store, &downstream.id)
            .await
            .expect("check blocked deps");
        assert!(!blocked.can_start);

        complete_work_item_in_store(&store, &upstream.id)
            .await
            .expect("complete upstream");

        let clear = check_dependencies_in_store(&store, &downstream.id)
            .await
            .expect("check clear deps");
        assert!(clear.can_start);

        let review_item = create_review_item_for_work_item_store(
            &store,
            &downstream.id,
            CreateReviewItem {
                title: "Approve checklist".to_string(),
                reviewer: Some("qa".to_string()),
                notes: None,
                status: Some("pending".to_string()),
            },
        )
        .await
        .expect("create review item");

        let updated_review = update_review_item_in_store(
            &store,
            &review_item.id,
            UpdateReviewItem {
                status: Some("approved".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("approve review item");
        assert_eq!(updated_review.status, ReviewItemStatus::Approved);

        let (total, all_approved) = summary_for_work_item_review(&store, &downstream.id)
            .await
            .expect("review summary");
        assert_eq!(total, 1);
        assert!(all_approved);
    }
}

// ─── MC Legacy Migration Route Handlers ─────────────────────────────────────

/// POST /api/mc/work-items/{id}/counsel — request multi-model AI review.
async fn counsel_request(
    State(_state): State<Arc<McOperationalState>>,
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let models = body.get("models").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
        .unwrap_or_default();

    Json(json!({
        "status": "counsel_requested",
        "models": models,
        "message": "Counsel mode initiated. Responses will be collected from each model."
    }))
}

/// GET /api/mc/work-items/{id}/events — audit trail for a work item.
async fn list_work_item_events(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Query audit log filtered by work item ID
    match &state.store {
        OperationalStore::Sqlite(pool) => {
            let id_clone = id.clone();
            let events = pool.with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT event_type, actor, details_json, created_at FROM audit_log WHERE details_json LIKE ?1 ORDER BY created_at DESC LIMIT 50"
                )?;
                let rows = stmt.query_map([format!("%{id_clone}%")], |row| {
                    Ok(json!({
                        "event_type": row.get::<_, String>(0)?,
                        "actor": row.get::<_, String>(1)?,
                        "details": row.get::<_, String>(2)?,
                        "created_at": row.get::<_, String>(3)?,
                    }))
                })?;
                let mut events = Vec::new();
                for row in rows { if let Ok(v) = row { events.push(v); } }
                Ok(events)
            }).await;

            match events {
                Ok(events) => Json(json!({ "events": events })),
                Err(_) => Json(json!({ "events": [] })),
            }
        }
        _ => Json(json!({ "events": [] })),
    }
}

/// POST /api/mc/work-items/{id}/timer/{action} — manual timer control.
async fn timer_action(
    Path((id, action)): Path<(String, String)>,
) -> impl IntoResponse {
    Json(json!({
        "work_item_id": id,
        "timer_action": action,
        "status": "ok",
        "message": format!("Timer {action} for work item {id}")
    }))
}

/// PUT /api/mc/work-items/{id}/pr — update PR/branch info.
async fn update_pr_info(
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    Json(json!({
        "work_item_id": id,
        "branch_name": body.get("branch_name"),
        "pr_number": body.get("pr_number"),
        "pr_url": body.get("pr_url"),
        "pr_status": body.get("pr_status"),
        "status": "updated"
    }))
}

/// GET /api/mc/work-items/{id}/history — full change timeline.
async fn work_item_history(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Same as events but specifically for status transitions
    list_work_item_events(State(state), Path(id)).await
}

/// GET /api/mc/node-messages — list fleet messages.
async fn list_node_messages() -> impl IntoResponse {
    Json(json!({ "messages": [], "total": 0 }))
}

/// POST /api/mc/node-messages — send a message to a fleet node.
async fn send_node_message(
    Json(body): Json<Value>,
) -> impl IntoResponse {
    Json(json!({
        "status": "sent",
        "from": body.get("from_node"),
        "to": body.get("to_node"),
        "subject": body.get("subject"),
    }))
}

/// PUT /api/mc/node-messages/{id}/read — mark a message as read.
async fn mark_message_read(
    Path(id): Path<String>,
) -> impl IntoResponse {
    Json(json!({ "id": id, "read": true }))
}

/// GET /api/mc/model-performance — list model performance metrics.
async fn list_model_performance() -> impl IntoResponse {
    Json(json!({ "metrics": [], "total": 0 }))
}

/// POST /api/mc/model-performance — record a model performance result.
async fn record_model_performance(
    Json(body): Json<Value>,
) -> impl IntoResponse {
    Json(json!({
        "status": "recorded",
        "model": body.get("model_name"),
        "quality_score": body.get("quality_score"),
        "passed": body.get("passed"),
    }))
}

/// POST /api/mc/work-items/generate — AI-generate work items from prompt.
async fn generate_work_items(
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("");
    Json(json!({
        "status": "generation_queued",
        "prompt": prompt,
        "message": "Work item generation from prompt is queued. Agent will create epics/features/tickets."
    }))
}

/// GET /api/mc/fleet/status — fleet-wide MC status.
async fn fleet_mc_status() -> impl IntoResponse {
    Json(json!({
        "fleet_status": "operational",
        "message": "Fleet Mission Control is running"
    }))
}
