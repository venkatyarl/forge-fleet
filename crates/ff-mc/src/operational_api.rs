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
use tracing::{info, warn};
use uuid::Uuid;

use crate::board::{BoardColumn, BoardFilter, BoardView};
use crate::dashboard::{BlockedItemSummary, DashboardStats, OverdueItemSummary, VelocityPoint};
use crate::dependency::{DependencyCheck, WorkItemDependency};
use crate::error::{McError, McResult};
use crate::legal::{
    ComplianceObligation, CreateComplianceObligation, CreateFiling, CreateLegalEntity, Filing,
    FilingDueItem, FilingFilter, FilingStatus, LegalEntity, UpdateComplianceObligation,
    UpdateFiling, UpdateLegalEntity,
};
use crate::operational_portfolio::{
    assign_task_group_item, config_kv_delete, config_kv_get, config_kv_list, config_kv_set,
    create_company, create_epic, create_project, create_project_environment, create_project_repo,
    create_sprint, create_task_group, delete_company, delete_epic, delete_project, delete_sprint,
    delete_task_group, get_company, get_epic, get_epic_progress, get_portfolio_summary,
    get_project, get_sprint, get_sprint_burndown, get_sprint_stats, get_task_group, list_companies,
    list_epics, list_project_environments, list_project_repos, list_projects, list_sprints,
    list_task_group_items, list_task_groups, unassign_task_group_item, update_company, update_epic,
    update_project, update_sprint, update_task_group,
};
use crate::review_item::{CreateReviewItem, ReviewItem, ReviewItemStatus, UpdateReviewItem};
use crate::sprint::Sprint;
use crate::work_item::{
    CreateWorkItem, Priority, UpdateWorkItem, WorkItem, WorkItemFilter, WorkItemStatus,
};

pub const WORK_ITEM_KEY_PREFIX: &str = "ff_mc.work_item.";
const REVIEW_ITEM_KEY_PREFIX: &str = "ff_mc.review_item.";
const DEPENDENCY_KEY_PREFIX: &str = "ff_mc.dependency.";
const LEGAL_ENTITY_KEY_PREFIX: &str = "ff_mc.legal_entity.";
const OBLIGATION_KEY_PREFIX: &str = "ff_mc.compliance_obligation.";
const FILING_KEY_PREFIX: &str = "ff_mc.filing.";
const SPRINT_KEY_PREFIX: &str = "ff_mc.sprint.";
const WORK_ITEM_EVENT_KEY_PREFIX: &str = "ff_mc.work_item_event.";
const WORK_ITEM_META_KEY_PREFIX: &str = "ff_mc.work_item_meta.";
const STORE_SCAN_LIMIT: u32 = 20_000;

#[derive(Debug, Clone)]
pub struct McOperationalState {
    pub store: OperationalStore,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    error: String,
}

pub fn mc_error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

fn handle_mc_err(e: McError) -> impl IntoResponse {
    match &e {
        McError::WorkItemNotFound { .. }
        | McError::ReviewItemNotFound { .. }
        | McError::EpicNotFound { .. }
        | McError::SprintNotFound { .. }
        | McError::TaskGroupNotFound { .. }
        | McError::CompanyNotFound { .. }
        | McError::ProjectNotFound { .. }
        | McError::ProjectRepoNotFound { .. }
        | McError::ProjectEnvironmentNotFound { .. }
        | McError::LegalEntityNotFound { .. }
        | McError::ComplianceObligationNotFound { .. }
        | McError::FilingNotFound { .. } => mc_error(StatusCode::NOT_FOUND, e.to_string()),
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
        .route(
            "/api/mc/worker-messages",
            get(list_node_messages).post(send_node_message),
        )
        .route("/api/mc/worker-messages/{id}/read", put(mark_message_read))
        // Legacy aliases — drop once dashboard rolls over.
        .route(
            "/api/mc/node-messages",
            get(list_node_messages).post(send_node_message),
        )
        .route("/api/mc/node-messages/{id}/read", put(mark_message_read))
        .route(
            "/api/mc/model-performance",
            get(list_model_performance).post(record_model_performance),
        )
        .route("/api/mc/work-items/generate", post(generate_work_items))
        .route("/api/mc/fleet/status", get(fleet_mc_status))
        // ─── Legal / Compliance routes ─────────────────────────────
        .route("/api/mc/legal/entities", get(list_legal_entities))
        .route("/api/mc/legal/entities", post(create_legal_entity))
        .route("/api/mc/legal/entities/{id}", get(get_legal_entity))
        .route("/api/mc/legal/entities/{id}", patch(update_legal_entity))
        .route("/api/mc/legal/entities/{id}", delete(delete_legal_entity))
        .route("/api/mc/legal/obligations", get(list_obligations))
        .route("/api/mc/legal/obligations", post(create_obligation))
        .route("/api/mc/legal/obligations/{id}", get(get_obligation))
        .route("/api/mc/legal/obligations/{id}", patch(update_obligation))
        .route("/api/mc/legal/obligations/{id}", delete(delete_obligation))
        .route("/api/mc/legal/filings/due-soon", get(list_due_soon_filings))
        .route("/api/mc/legal/filings", get(list_filings))
        .route("/api/mc/legal/filings", post(create_filing))
        .route("/api/mc/legal/filings/{id}", get(get_filing))
        .route("/api/mc/legal/filings/{id}", patch(update_filing))
        .route("/api/mc/legal/filings/{id}", delete(delete_filing))
        // ─── Portfolio + Planning routes ─────────────────────────────
        .route(
            "/api/mc/companies",
            get(list_companies).post(create_company),
        )
        .route(
            "/api/mc/companies/{id}",
            get(get_company)
                .patch(update_company)
                .delete(delete_company),
        )
        .route("/api/mc/projects", get(list_projects).post(create_project))
        .route(
            "/api/mc/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route(
            "/api/mc/projects/{id}/repos",
            get(list_project_repos).post(create_project_repo),
        )
        .route(
            "/api/mc/projects/{id}/environments",
            get(list_project_environments).post(create_project_environment),
        )
        .route("/api/mc/portfolio/summary", get(get_portfolio_summary))
        .route("/api/mc/epics", get(list_epics).post(create_epic))
        .route(
            "/api/mc/epics/{id}",
            get(get_epic).patch(update_epic).delete(delete_epic),
        )
        .route("/api/mc/epics/{id}/progress", get(get_epic_progress))
        .route("/api/mc/sprints", get(list_sprints).post(create_sprint))
        .route(
            "/api/mc/sprints/{id}",
            get(get_sprint).patch(update_sprint).delete(delete_sprint),
        )
        .route("/api/mc/sprints/{id}/stats", get(get_sprint_stats))
        .route("/api/mc/sprints/{id}/burndown", get(get_sprint_burndown))
        .route(
            "/api/mc/task-groups",
            get(list_task_groups).post(create_task_group),
        )
        .route(
            "/api/mc/task-groups/{id}",
            get(get_task_group)
                .patch(update_task_group)
                .delete(delete_task_group),
        )
        .route("/api/mc/task-groups/{id}/items", get(list_task_group_items))
        .route(
            "/api/mc/task-groups/{id}/items/{work_item_id}",
            post(assign_task_group_item),
        )
        .route(
            "/api/mc/task-groups/{id}/items/{work_item_id}",
            delete(unassign_task_group_item),
        )
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
        Ok(item) => {
            info!(work_item_id = %item.id, title = %item.title, status = %item.status, "created work item");
            (StatusCode::CREATED, Json(item)).into_response()
        }
        Err(err) => {
            warn!(error = %err, "failed creating work item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn get_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match get_work_item_from_store(&state.store, &id).await {
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, "got work item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed getting work item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn update_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateWorkItem>,
) -> impl IntoResponse {
    match update_work_item_in_store(&state.store, &id, params).await {
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, "updated work item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed updating work item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn delete_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match delete_work_item_in_store(&state.store, &id).await {
        Ok(()) => {
            info!(work_item_id = %id, "deleted work item");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed deleting work item");
            handle_mc_err(err).into_response()
        }
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
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, assignee = %item.assignee, "claimed work item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed claiming work item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn complete_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match complete_work_item_in_store(&state.store, &id).await {
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, "completed work item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed completing work item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn fail_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match fail_work_item_in_store(&state.store, &id).await {
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, "failed work item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed marking work item failed");
            handle_mc_err(err).into_response()
        }
    }
}

async fn escalate_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match escalate_work_item_in_store(&state.store, &id).await {
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, priority = item.priority.0, "escalated work item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed escalating work item");
            handle_mc_err(err).into_response()
        }
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
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, "submitted work item for review");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed submitting work item for review");
            handle_mc_err(err).into_response()
        }
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
                warn!(work_item_id = %id, total_items, all_approved, "cannot complete review: checklist has non-approved items");
                return handle_mc_err(McError::Other(anyhow!(
                    "cannot complete review: checklist has non-approved items"
                )))
                .into_response();
            }
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed summarizing work item review");
            return handle_mc_err(err).into_response();
        }
    }

    match complete_work_item_in_store(&state.store, &id).await {
        Ok(item) => {
            info!(work_item_id = %id, status = %item.status, "completed work item review");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed completing work item review");
            handle_mc_err(err).into_response()
        }
    }
}

async fn list_review_items_for_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match list_review_items_for_work_item_store(&state.store, &id).await {
        Ok(items) => {
            info!(work_item_id = %id, count = items.len(), "listed review items");
            Json(items).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed listing review items");
            handle_mc_err(err).into_response()
        }
    }
}

async fn create_review_item_for_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<CreateReviewItem>,
) -> impl IntoResponse {
    match create_review_item_for_work_item_store(&state.store, &id, params).await {
        Ok(item) => {
            info!(work_item_id = %id, review_item_id = %item.id, status = %item.status, "created review item");
            (StatusCode::CREATED, Json(item)).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed creating review item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn update_review_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateReviewItem>,
) -> impl IntoResponse {
    match update_review_item_in_store(&state.store, &id, params).await {
        Ok(item) => {
            info!(review_item_id = %id, work_item_id = %item.work_item_id, status = %item.status, "updated review item");
            Json(item).into_response()
        }
        Err(err) => {
            warn!(review_item_id = %id, error = %err, "failed updating review item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn delete_review_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match delete_review_item_in_store(&state.store, &id).await {
        Ok(()) => {
            info!(review_item_id = %id, "deleted review item");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            warn!(review_item_id = %id, error = %err, "failed deleting review item");
            handle_mc_err(err).into_response()
        }
    }
}

async fn reset_review_items_for_work_item(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match reset_review_items_for_work_item_store(&state.store, &id).await {
        Ok(updated) => {
            info!(work_item_id = %id, updated, "reset review items");
            Json(serde_json::json!({"updated": updated})).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed resetting review items");
            handle_mc_err(err).into_response()
        }
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
        Ok(deps) => {
            info!(work_item_id = %id, count = deps.len(), "listed work item dependencies");
            Json(deps).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed listing work item dependencies");
            handle_mc_err(err).into_response()
        }
    }
}

async fn add_work_item_dependency(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(body): Json<AddDependencyRequest>,
) -> impl IntoResponse {
    match add_dependency_in_store(&state.store, &id, &body.depends_on_id).await {
        Ok(dep) => {
            info!(work_item_id = %id, depends_on_id = %dep.depends_on_id, "added work item dependency");
            (StatusCode::CREATED, Json(dep)).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, depends_on_id = %body.depends_on_id, error = %err, "failed adding work item dependency");
            handle_mc_err(err).into_response()
        }
    }
}

async fn remove_work_item_dependency(
    State(state): State<Arc<McOperationalState>>,
    Path((id, depends_on_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match remove_dependency_in_store(&state.store, &id, &depends_on_id).await {
        Ok(true) => {
            info!(work_item_id = %id, depends_on_id = %depends_on_id, "removed work item dependency");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            warn!(work_item_id = %id, depends_on_id = %depends_on_id, "dependency not found for removal");
            mc_error(StatusCode::NOT_FOUND, "dependency not found").into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, depends_on_id = %depends_on_id, error = %err, "failed removing work item dependency");
            handle_mc_err(err).into_response()
        }
    }
}

async fn check_work_item_dependencies(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match check_dependencies_in_store(&state.store, &id).await {
        Ok(result) => {
            info!(
                work_item_id = %id,
                blocked_count = result.blocked_count,
                can_start = result.can_start,
                "checked work item dependencies"
            );
            Json(result).into_response()
        }
        Err(err) => {
            warn!(work_item_id = %id, error = %err, "failed checking work item dependencies");
            handle_mc_err(err).into_response()
        }
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

pub fn to_other_error(context: &str, err: impl std::fmt::Display) -> McError {
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

/// Load a work item from the canonical operational store.
pub async fn get_work_item_from_store(store: &OperationalStore, id: &str) -> McResult<WorkItem> {
    let key = work_item_storage_key(id);
    let Some(payload) = store
        .config_get(&key)
        .await
        .map_err(|e| to_other_error("failed loading work item", e))?
    else {
        warn!(work_item_id = %id, "work item not found in store");
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
        .map_err(|e| to_other_error("failed saving work item", e))?;
    info!(work_item_id = %item.id, status = %item.status, "persisted work item");
    Ok(())
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
        eisenhower_quadrant: None,
        numeric_priority: None,
        pick_score: None,
        capability_tags: Vec::new(),
        context: params.context,
        created_at: now,
        updated_at: now,
    };

    persist_work_item(store, &item).await?;
    info!(work_item_id = %item.id, title = %item.title, status = %item.status, "created work item in store");
    Ok(item)
}

async fn update_work_item_in_store(
    store: &OperationalStore,
    id: &str,
    params: UpdateWorkItem,
) -> McResult<WorkItem> {
    let mut item = get_work_item_from_store(store, id).await?;
    let old_status = item.status;

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
    if let Some(context) = params.context {
        item.context = context;
    }

    item.updated_at = Utc::now();
    persist_work_item(store, &item).await?;
    if item.status != old_status {
        info!(
            work_item_id = %id,
            old_status = %old_status,
            new_status = %item.status,
            "work item status transitioned"
        );
    } else {
        info!(work_item_id = %id, status = %item.status, "updated work item in store");
    }
    Ok(item)
}

async fn delete_work_item_in_store(store: &OperationalStore, id: &str) -> McResult<()> {
    let deleted = store
        .config_delete(&work_item_storage_key(id))
        .await
        .map_err(|e| to_other_error("failed deleting work item", e))?;

    if !deleted {
        warn!(work_item_id = %id, "work item not found for deletion");
        return Err(McError::WorkItemNotFound { id: id.to_string() });
    }

    // Cleanup child records to keep parity with sqlite's ON DELETE behavior.
    delete_review_items_for_work_item_store(store, id).await?;
    delete_dependencies_for_work_item_store(store, id).await?;

    info!(work_item_id = %id, "deleted work item from store");
    Ok(())
}

async fn claim_work_item_in_store(
    store: &OperationalStore,
    id: &str,
    assignee: Option<String>,
) -> McResult<WorkItem> {
    let item = get_work_item_from_store(store, id).await?;
    let new_assignee = assignee.unwrap_or_else(|| "unassigned".to_string());
    let mut update = UpdateWorkItem {
        assignee: Some(new_assignee.clone()),
        ..Default::default()
    };

    if item.status == WorkItemStatus::Backlog {
        update.status = Some("todo".to_string());
    }

    info!(
        work_item_id = %id,
        old_status = %item.status,
        assignee = %new_assignee,
        "claiming work item"
    );
    update_work_item_in_store(store, id, update).await
}

async fn complete_work_item_in_store(store: &OperationalStore, id: &str) -> McResult<WorkItem> {
    info!(work_item_id = %id, "completing work item");
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
    info!(work_item_id = %id, "failing work item");
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

    info!(
        work_item_id = %id,
        old_priority = item.priority.0,
        new_priority,
        old_status = %item.status,
        "escalating work item"
    );
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

    let velocity_trend = compute_velocity_trend_from_store(store).await?;
    let overdue_items = compute_overdue_items_from_store(store).await?;

    Ok(DashboardStats {
        total_items: items.len() as i64,
        items_by_status,
        items_per_assignee,
        blocked_items,
        velocity_trend,
        overdue_items,
    })
}

async fn compute_velocity_trend_from_store(
    store: &OperationalStore,
) -> McResult<Vec<VelocityPoint>> {
    let mut sprints = config_kv_list::<Sprint>(store, SPRINT_KEY_PREFIX).await?;
    sprints.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let mut trend = Vec::new();
    for sprint in sprints.into_iter().take(10) {
        let items = list_work_items_from_store(
            store,
            &WorkItemFilter {
                sprint_id: Some(sprint.id.clone()),
                ..Default::default()
            },
        )
        .await?;
        let done_items = items
            .iter()
            .filter(|item| item.status == WorkItemStatus::Done)
            .count();

        trend.push(VelocityPoint {
            sprint_id: sprint.id,
            sprint_name: sprint.name,
            done_items,
            total_items: items.len(),
        });
    }

    Ok(trend)
}

async fn compute_overdue_items_from_store(
    store: &OperationalStore,
) -> McResult<Vec<OverdueItemSummary>> {
    let today = Utc::now().date_naive();
    let mut sprints = config_kv_list::<Sprint>(store, SPRINT_KEY_PREFIX).await?;
    sprints.sort_by(|a, b| {
        a.end_date
            .cmp(&b.end_date)
            .then_with(|| a.created_at.cmp(&b.created_at))
    });

    let mut overdue = Vec::new();
    for sprint in sprints {
        let Some(end_date) = sprint.end_date.filter(|date| *date < today) else {
            continue;
        };

        let items = list_work_items_from_store(
            store,
            &WorkItemFilter {
                sprint_id: Some(sprint.id.clone()),
                ..Default::default()
            },
        )
        .await?;

        for item in items {
            if item.status != WorkItemStatus::Done {
                overdue.push(OverdueItemSummary {
                    id: item.id,
                    title: item.title,
                    sprint_id: sprint.id.clone(),
                    sprint_name: sprint.name.clone(),
                    sprint_end_date: end_date.to_string(),
                    status: item.status.as_str().to_string(),
                });
            }
        }
    }

    Ok(overdue)
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
    info!(
        review_item_id = %item.id,
        work_item_id = %item.work_item_id,
        status = %item.status,
        "created review item"
    );
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
    info!(
        review_item_id = %item.id,
        work_item_id = %item.work_item_id,
        status = %item.status,
        "updated review item"
    );
    Ok(item)
}

async fn delete_review_item_in_store(store: &OperationalStore, id: &str) -> McResult<()> {
    let (key, _item) = find_review_item_record(store, id).await?;

    let deleted = store
        .config_delete(&key)
        .await
        .map_err(|e| to_other_error("failed deleting review item", e))?;

    if !deleted {
        warn!(review_item_id = %id, "review item not found for deletion");
        return Err(McError::ReviewItemNotFound { id: id.to_string() });
    }

    info!(review_item_id = %id, "deleted review item");
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

    if updated > 0 {
        info!(work_item_id = %work_item_id, updated, "reset review items to pending");
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
        .map_err(|e| to_other_error("failed saving review item", e))?;
    info!(
        review_item_id = %item.id,
        work_item_id = %item.work_item_id,
        status = %item.status,
        "persisted review item"
    );
    Ok(())
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

    info!(work_item_id = %work_item_id, depends_on_id = %depends_on_id, "added work item dependency");
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
    let deleted = store
        .config_delete(&dependency_storage_key(work_item_id, depends_on_id))
        .await
        .map_err(|e| to_other_error("failed deleting dependency", e))?;
    if deleted {
        info!(work_item_id = %work_item_id, depends_on_id = %depends_on_id, "removed dependency from store");
    }
    Ok(deleted)
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

    info!(
        work_item_id = %work_item_id,
        blocked_count,
        can_start = blocked_count == 0,
        "checked dependencies"
    );

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

// ─── Legal / Compliance persistence helpers ────────────────────────────────

fn normalize_legal_status(raw: Option<String>, default: &str) -> String {
    raw.unwrap_or_else(|| default.to_string())
        .trim()
        .to_lowercase()
        .replace([' ', '-'], "_")
}

#[derive(Debug, Deserialize)]
struct ObligationQuery {
    entity_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FilingQueryParams {
    entity_id: Option<String>,
    obligation_id: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DueSoonQuery {
    days: Option<i64>,
}

async fn list_legal_entities(State(state): State<Arc<McOperationalState>>) -> impl IntoResponse {
    match config_kv_list::<LegalEntity>(&state.store, LEGAL_ENTITY_KEY_PREFIX).await {
        Ok(mut items) => {
            items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Json(items).into_response()
        }
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn create_legal_entity(
    State(state): State<Arc<McOperationalState>>,
    Json(params): Json<CreateLegalEntity>,
) -> impl IntoResponse {
    let now = Utc::now();
    let item = LegalEntity {
        id: Uuid::new_v4().to_string(),
        name: params.name,
        entity_type: params.entity_type,
        jurisdiction: params.jurisdiction,
        registration_number: params.registration_number,
        status: normalize_legal_status(params.status, "active"),
        created_at: now,
        updated_at: now,
    };

    match config_kv_set(
        &state.store,
        &format!("{LEGAL_ENTITY_KEY_PREFIX}{}", item.id),
        &item,
    )
    .await
    {
        Ok(()) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn get_legal_entity(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match config_kv_get::<LegalEntity>(&state.store, &format!("{LEGAL_ENTITY_KEY_PREFIX}{id}"))
        .await
    {
        Ok(Some(item)) => Json(item).into_response(),
        Ok(None) => handle_mc_err(McError::LegalEntityNotFound { id }).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn update_legal_entity(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateLegalEntity>,
) -> impl IntoResponse {
    let key = format!("{LEGAL_ENTITY_KEY_PREFIX}{id}");
    let mut item = match config_kv_get::<LegalEntity>(&state.store, &key).await {
        Ok(Some(item)) => item,
        Ok(None) => return handle_mc_err(McError::LegalEntityNotFound { id }).into_response(),
        Err(err) => return handle_mc_err(err).into_response(),
    };

    if let Some(name) = params.name {
        item.name = name;
    }
    if let Some(entity_type) = params.entity_type {
        item.entity_type = entity_type;
    }
    if let Some(jurisdiction) = params.jurisdiction {
        item.jurisdiction = jurisdiction;
    }
    if let Some(registration_number) = params.registration_number {
        item.registration_number = registration_number;
    }
    if let Some(status) = params.status {
        item.status = normalize_legal_status(Some(status), "active");
    }
    item.updated_at = Utc::now();

    match config_kv_set(&state.store, &key, &item).await {
        Ok(()) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn delete_legal_entity(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let key = format!("{LEGAL_ENTITY_KEY_PREFIX}{id}");
    match config_kv_delete(&state.store, &key).await {
        Ok(true) => {
            if let Ok(obligations) =
                config_kv_list::<ComplianceObligation>(&state.store, OBLIGATION_KEY_PREFIX).await
            {
                for obligation in obligations.into_iter().filter(|o| o.entity_id == id) {
                    let _ = config_kv_delete(
                        &state.store,
                        &format!("{OBLIGATION_KEY_PREFIX}{}", obligation.id),
                    )
                    .await;
                }
            }
            if let Ok(filings) = config_kv_list::<Filing>(&state.store, FILING_KEY_PREFIX).await {
                for filing in filings.into_iter().filter(|f| f.entity_id == id) {
                    let _ = config_kv_delete(
                        &state.store,
                        &format!("{FILING_KEY_PREFIX}{}", filing.id),
                    )
                    .await;
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => handle_mc_err(McError::LegalEntityNotFound { id }).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn list_obligations(
    State(state): State<Arc<McOperationalState>>,
    Query(q): Query<ObligationQuery>,
) -> impl IntoResponse {
    match config_kv_list::<ComplianceObligation>(&state.store, OBLIGATION_KEY_PREFIX).await {
        Ok(mut items) => {
            if let Some(entity_id) = q.entity_id {
                items.retain(|item| item.entity_id == entity_id);
            }
            items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Json(items).into_response()
        }
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn create_obligation(
    State(state): State<Arc<McOperationalState>>,
    Json(params): Json<CreateComplianceObligation>,
) -> impl IntoResponse {
    match config_kv_get::<LegalEntity>(
        &state.store,
        &format!("{LEGAL_ENTITY_KEY_PREFIX}{}", params.entity_id),
    )
    .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            return handle_mc_err(McError::LegalEntityNotFound {
                id: params.entity_id.clone(),
            })
            .into_response();
        }
        Err(err) => return handle_mc_err(err).into_response(),
    }

    let now = Utc::now();
    let item = ComplianceObligation {
        id: Uuid::new_v4().to_string(),
        entity_id: params.entity_id,
        title: params.title,
        description: params.description,
        jurisdiction: params.jurisdiction,
        frequency: params.frequency.unwrap_or_else(|| "annual".to_string()),
        status: normalize_legal_status(params.status, "active"),
        created_at: now,
        updated_at: now,
    };

    match config_kv_set(
        &state.store,
        &format!("{OBLIGATION_KEY_PREFIX}{}", item.id),
        &item,
    )
    .await
    {
        Ok(()) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn get_obligation(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match config_kv_get::<ComplianceObligation>(
        &state.store,
        &format!("{OBLIGATION_KEY_PREFIX}{id}"),
    )
    .await
    {
        Ok(Some(item)) => Json(item).into_response(),
        Ok(None) => handle_mc_err(McError::ComplianceObligationNotFound { id }).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn update_obligation(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateComplianceObligation>,
) -> impl IntoResponse {
    let key = format!("{OBLIGATION_KEY_PREFIX}{id}");
    let mut item = match config_kv_get::<ComplianceObligation>(&state.store, &key).await {
        Ok(Some(item)) => item,
        Ok(None) => {
            return handle_mc_err(McError::ComplianceObligationNotFound { id }).into_response();
        }
        Err(err) => return handle_mc_err(err).into_response(),
    };

    if let Some(title) = params.title {
        item.title = title;
    }
    if let Some(description) = params.description {
        item.description = description;
    }
    if let Some(jurisdiction) = params.jurisdiction {
        item.jurisdiction = jurisdiction;
    }
    if let Some(frequency) = params.frequency {
        item.frequency = frequency;
    }
    if let Some(status) = params.status {
        item.status = normalize_legal_status(Some(status), "active");
    }
    item.updated_at = Utc::now();

    match config_kv_set(&state.store, &key, &item).await {
        Ok(()) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn delete_obligation(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let key = format!("{OBLIGATION_KEY_PREFIX}{id}");
    match config_kv_delete(&state.store, &key).await {
        Ok(true) => {
            if let Ok(filings) = config_kv_list::<Filing>(&state.store, FILING_KEY_PREFIX).await {
                for mut filing in filings
                    .into_iter()
                    .filter(|f| f.obligation_id.as_deref() == Some(&id))
                {
                    filing.obligation_id = None;
                    filing.updated_at = Utc::now();
                    let _ = config_kv_set(
                        &state.store,
                        &format!("{FILING_KEY_PREFIX}{}", filing.id),
                        &filing,
                    )
                    .await;
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => handle_mc_err(McError::ComplianceObligationNotFound { id }).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn list_filings(
    State(state): State<Arc<McOperationalState>>,
    Query(q): Query<FilingQueryParams>,
) -> impl IntoResponse {
    let status = match q.status {
        Some(raw) => match FilingStatus::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(err) => return handle_mc_err(err).into_response(),
        },
        None => None,
    };
    let filter = FilingFilter {
        entity_id: q.entity_id,
        obligation_id: q.obligation_id,
        status,
    };

    match list_filings_from_store(&state.store, &filter).await {
        Ok(items) => Json(items).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn create_filing(
    State(state): State<Arc<McOperationalState>>,
    Json(params): Json<CreateFiling>,
) -> impl IntoResponse {
    match config_kv_get::<LegalEntity>(
        &state.store,
        &format!("{LEGAL_ENTITY_KEY_PREFIX}{}", params.entity_id),
    )
    .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            return handle_mc_err(McError::LegalEntityNotFound {
                id: params.entity_id.clone(),
            })
            .into_response();
        }
        Err(err) => return handle_mc_err(err).into_response(),
    }
    if let Some(obligation_id) = &params.obligation_id {
        match config_kv_get::<ComplianceObligation>(
            &state.store,
            &format!("{OBLIGATION_KEY_PREFIX}{obligation_id}"),
        )
        .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return handle_mc_err(McError::ComplianceObligationNotFound {
                    id: obligation_id.clone(),
                })
                .into_response();
            }
            Err(err) => return handle_mc_err(err).into_response(),
        }
    }

    let status = match params.status {
        Some(raw) => match FilingStatus::from_str_loose(&raw) {
            Ok(status) => status,
            Err(err) => return handle_mc_err(err).into_response(),
        },
        None => FilingStatus::Pending,
    };
    let now = Utc::now();
    let item = Filing {
        id: Uuid::new_v4().to_string(),
        entity_id: params.entity_id,
        obligation_id: params.obligation_id,
        jurisdiction: params.jurisdiction,
        due_date: params.due_date,
        status,
        filed_on: params.filed_on,
        notes: params.notes,
        created_at: now,
        updated_at: now,
    };

    match config_kv_set(
        &state.store,
        &format!("{FILING_KEY_PREFIX}{}", item.id),
        &item,
    )
    .await
    {
        Ok(()) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn get_filing(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match config_kv_get::<Filing>(&state.store, &format!("{FILING_KEY_PREFIX}{id}")).await {
        Ok(Some(item)) => Json(item).into_response(),
        Ok(None) => handle_mc_err(McError::FilingNotFound { id }).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn update_filing(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateFiling>,
) -> impl IntoResponse {
    let key = format!("{FILING_KEY_PREFIX}{id}");
    let mut item = match config_kv_get::<Filing>(&state.store, &key).await {
        Ok(Some(item)) => item,
        Ok(None) => return handle_mc_err(McError::FilingNotFound { id }).into_response(),
        Err(err) => return handle_mc_err(err).into_response(),
    };

    if let Some(obligation_id) = params.obligation_id {
        item.obligation_id = obligation_id;
    }
    if let Some(jurisdiction) = params.jurisdiction {
        item.jurisdiction = jurisdiction;
    }
    if let Some(due_date) = params.due_date {
        item.due_date = due_date;
    }
    if let Some(status) = params.status {
        item.status = match FilingStatus::from_str_loose(&status) {
            Ok(status) => status,
            Err(err) => return handle_mc_err(err).into_response(),
        };
    }
    if let Some(filed_on) = params.filed_on {
        item.filed_on = filed_on;
    }
    if let Some(notes) = params.notes {
        item.notes = notes;
    }
    item.updated_at = Utc::now();

    match config_kv_set(&state.store, &key, &item).await {
        Ok(()) => Json(item).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn delete_filing(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let key = format!("{FILING_KEY_PREFIX}{id}");
    match config_kv_delete(&state.store, &key).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => handle_mc_err(McError::FilingNotFound { id }).into_response(),
        Err(err) => handle_mc_err(err).into_response(),
    }
}

async fn list_due_soon_filings(
    State(state): State<Arc<McOperationalState>>,
    Query(q): Query<DueSoonQuery>,
) -> impl IntoResponse {
    let days = q.days.unwrap_or(30).clamp(0, 365);
    let today = Utc::now().date_naive();
    let end_date = today + chrono::Duration::days(days);

    let filings = match list_filings_from_store(&state.store, &FilingFilter::default()).await {
        Ok(items) => items,
        Err(err) => return handle_mc_err(err).into_response(),
    };
    let entities = config_kv_list::<LegalEntity>(&state.store, LEGAL_ENTITY_KEY_PREFIX)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|entity| (entity.id.clone(), entity))
        .collect::<HashMap<_, _>>();
    let obligations = config_kv_list::<ComplianceObligation>(&state.store, OBLIGATION_KEY_PREFIX)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|obligation| (obligation.id.clone(), obligation))
        .collect::<HashMap<_, _>>();

    let items = select_due_soon_filings(filings, &entities, &obligations, today, end_date);
    Json(items).into_response()
}

/// Build the "due soon" filing list: every still-outstanding filing due on or
/// before `end_date`, **including already-overdue ones** (`due_date < today`) so
/// a passed compliance deadline never silently drops off the radar — the old
/// `due_date >= today` lower bound hid filings exactly when they became most
/// urgent. Resolved filings (`Filed` / `Waived`) are excluded. Sorted by
/// `due_date` ascending (most-overdue first), then `created_at`. Overdue items
/// carry a negative `days_until_due`.
fn select_due_soon_filings(
    filings: Vec<Filing>,
    entities: &HashMap<String, LegalEntity>,
    obligations: &HashMap<String, ComplianceObligation>,
    today: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
) -> Vec<FilingDueItem> {
    let mut items = filings
        .into_iter()
        .filter(|filing| filing.due_date <= end_date)
        .filter(|filing| !matches!(filing.status, FilingStatus::Filed | FilingStatus::Waived))
        .filter_map(|filing| {
            let entity_name = entities.get(&filing.entity_id)?.name.clone();
            let obligation_title = filing
                .obligation_id
                .as_ref()
                .and_then(|id| obligations.get(id))
                .map(|obligation| obligation.title.clone());
            let days_until_due = filing.due_date.signed_duration_since(today).num_days();
            Some(FilingDueItem {
                filing,
                entity_name,
                obligation_title,
                days_until_due,
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        a.filing
            .due_date
            .cmp(&b.filing.due_date)
            .then_with(|| a.filing.created_at.cmp(&b.filing.created_at))
    });
    items
}

async fn list_filings_from_store(
    store: &OperationalStore,
    filter: &FilingFilter,
) -> McResult<Vec<Filing>> {
    let mut items = config_kv_list::<Filing>(store, FILING_KEY_PREFIX).await?;
    if let Some(entity_id) = &filter.entity_id {
        items.retain(|item| &item.entity_id == entity_id);
    }
    if let Some(obligation_id) = &filter.obligation_id {
        items.retain(|item| item.obligation_id.as_ref() == Some(obligation_id));
    }
    if let Some(status) = filter.status {
        items.retain(|item| item.status == status);
    }
    items.sort_by(|a, b| {
        a.due_date
            .cmp(&b.due_date)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    Ok(items)
}

// ─── MC Legacy Migration Route Handlers ─────────────────────────────────────

fn not_implemented(feature: &str) -> impl IntoResponse {
    mc_error(
        StatusCode::NOT_IMPLEMENTED,
        format!("{feature} is not implemented by the config_kv operational store"),
    )
}

async fn append_work_item_event(
    store: &OperationalStore,
    work_item_id: &str,
    event_type: &str,
    details: Value,
) -> McResult<Value> {
    let event = json!({
        "id": Uuid::new_v4().to_string(),
        "work_item_id": work_item_id,
        "event_type": event_type,
        "actor": "mission-control",
        "details": details,
        "created_at": Utc::now(),
    });
    let key = format!(
        "{WORK_ITEM_EVENT_KEY_PREFIX}{work_item_id}.{}.{}",
        Utc::now().timestamp_micros(),
        event["id"].as_str().unwrap_or_default()
    );
    config_kv_set(store, &key, &event).await?;
    Ok(event)
}

async fn update_work_item_meta(
    store: &OperationalStore,
    work_item_id: &str,
    fields: Value,
) -> McResult<Value> {
    let key = format!("{WORK_ITEM_META_KEY_PREFIX}{work_item_id}");
    let mut meta = config_kv_get::<Value>(store, &key)
        .await?
        .unwrap_or_else(|| json!({ "work_item_id": work_item_id }));

    if let (Some(target), Some(source)) = (meta.as_object_mut(), fields.as_object()) {
        for (name, value) in source {
            target.insert(name.clone(), value.clone());
        }
        target.insert("updated_at".to_string(), json!(Utc::now()));
    }

    config_kv_set(store, &key, &meta).await?;
    Ok(meta)
}

/// POST /api/mc/work-items/{id}/counsel — request multi-model AI review.
async fn counsel_request(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if let Err(err) = get_work_item_from_store(&state.store, &id).await {
        return handle_mc_err(err).into_response();
    }

    let models = body
        .get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let meta = match update_work_item_meta(
        &state.store,
        &id,
        json!({
            "counsel_mode": true,
            "counsel_models": models,
            "counsel_requested_at": Utc::now(),
        }),
    )
    .await
    {
        Ok(meta) => meta,
        Err(err) => return handle_mc_err(err).into_response(),
    };
    let _ = append_work_item_event(
        &state.store,
        &id,
        "counsel_requested",
        json!({ "models": meta.get("counsel_models").cloned().unwrap_or_else(|| json!([])) }),
    )
    .await;

    Json(json!({
        "status": "counsel_requested",
        "work_item_id": id,
        "models": meta.get("counsel_models").cloned().unwrap_or_else(|| json!([])),
        "metadata": meta,
    }))
    .into_response()
}

/// GET /api/mc/work-items/{id}/events — audit trail for a work item.
async fn list_work_item_events(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = get_work_item_from_store(&state.store, &id).await {
        return handle_mc_err(err).into_response();
    }

    match config_kv_list::<Value>(&state.store, &format!("{WORK_ITEM_EVENT_KEY_PREFIX}{id}.")).await
    {
        Ok(mut events) => {
            events.sort_by(|a, b| {
                b.get("created_at")
                    .and_then(Value::as_str)
                    .cmp(&a.get("created_at").and_then(Value::as_str))
            });
            Json(json!({ "events": events })).into_response()
        }
        Err(err) => handle_mc_err(err).into_response(),
    }
}

/// POST /api/mc/work-items/{id}/timer/{action} — manual timer control.
async fn timer_action(
    State(state): State<Arc<McOperationalState>>,
    Path((id, action)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(err) = get_work_item_from_store(&state.store, &id).await {
        return handle_mc_err(err).into_response();
    }

    let normalized = action.trim().to_lowercase().replace('-', "_");
    let valid = matches!(
        normalized.as_str(),
        "start" | "pause" | "resume" | "stop" | "reset"
    );
    if !valid {
        return mc_error(
            StatusCode::BAD_REQUEST,
            format!("invalid timer action: {action}"),
        )
        .into_response();
    }

    let meta = match update_work_item_meta(
        &state.store,
        &id,
        json!({
            "manual_timer_state": normalized,
            "manual_timer_updated_at": Utc::now(),
        }),
    )
    .await
    {
        Ok(meta) => meta,
        Err(err) => return handle_mc_err(err).into_response(),
    };
    let _ = append_work_item_event(
        &state.store,
        &id,
        "timer_action",
        json!({ "timer_action": action }),
    )
    .await;

    Json(json!({
        "work_item_id": id,
        "timer_action": action,
        "status": "ok",
        "metadata": meta,
    }))
    .into_response()
}

/// PUT /api/mc/work-items/{id}/pr — update PR/branch info.
async fn update_pr_info(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if let Err(err) = get_work_item_from_store(&state.store, &id).await {
        return handle_mc_err(err).into_response();
    }

    let meta = match update_work_item_meta(
        &state.store,
        &id,
        json!({
            "branch_name": body.get("branch_name").cloned().unwrap_or(Value::Null),
            "pr_number": body.get("pr_number").cloned().unwrap_or(Value::Null),
            "pr_url": body.get("pr_url").cloned().unwrap_or(Value::Null),
            "pr_status": body.get("pr_status").cloned().unwrap_or(Value::Null),
        }),
    )
    .await
    {
        Ok(meta) => meta,
        Err(err) => return handle_mc_err(err).into_response(),
    };
    let _ = append_work_item_event(&state.store, &id, "pr_info_updated", body).await;

    Json(json!({
        "work_item_id": id,
        "branch_name": meta.get("branch_name").cloned().unwrap_or(Value::Null),
        "pr_number": meta.get("pr_number").cloned().unwrap_or(Value::Null),
        "pr_url": meta.get("pr_url").cloned().unwrap_or(Value::Null),
        "pr_status": meta.get("pr_status").cloned().unwrap_or(Value::Null),
        "status": "updated",
    }))
    .into_response()
}

/// GET /api/mc/work-items/{id}/history — full change timeline.
async fn work_item_history(
    State(state): State<Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    list_work_item_events(State(state), Path(id)).await
}

/// GET /api/mc/node-messages — list fleet messages.
async fn list_node_messages() -> impl IntoResponse {
    not_implemented("worker/node messages")
}

/// POST /api/mc/node-messages — send a message to a fleet node.
async fn send_node_message(Json(_body): Json<Value>) -> impl IntoResponse {
    not_implemented("worker/node messages")
}

/// PUT /api/mc/node-messages/{id}/read — mark a message as read.
async fn mark_message_read(Path(_id): Path<String>) -> impl IntoResponse {
    not_implemented("worker/node messages")
}

/// GET /api/mc/model-performance — list model performance metrics.
async fn list_model_performance() -> impl IntoResponse {
    not_implemented("model performance metrics")
}

/// POST /api/mc/model-performance — record a model performance result.
async fn record_model_performance(Json(_body): Json<Value>) -> impl IntoResponse {
    not_implemented("model performance metrics")
}

/// POST /api/mc/work-items/generate — AI-generate work items from prompt.
async fn generate_work_items(Json(_body): Json<Value>) -> impl IntoResponse {
    not_implemented("AI work item generation")
}

/// GET /api/mc/fleet/status — fleet-wide MC status.
async fn fleet_mc_status() -> impl IntoResponse {
    not_implemented("fleet Mission Control status")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn filing(id: &str, due: NaiveDate, status: FilingStatus) -> Filing {
        let ts = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        Filing {
            id: id.to_string(),
            entity_id: "ent1".to_string(),
            obligation_id: None,
            jurisdiction: "US".to_string(),
            due_date: due,
            status,
            filed_on: None,
            notes: None,
            created_at: ts,
            updated_at: ts,
        }
    }

    fn entities() -> HashMap<String, LegalEntity> {
        let ts = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut m = HashMap::new();
        m.insert(
            "ent1".to_string(),
            LegalEntity {
                id: "ent1".to_string(),
                name: "Acme LLC".to_string(),
                entity_type: "llc".to_string(),
                jurisdiction: "US".to_string(),
                registration_number: None,
                status: "active".to_string(),
                created_at: ts,
                updated_at: ts,
            },
        );
        m
    }

    #[test]
    fn due_soon_includes_overdue_and_excludes_resolved() {
        let today = date(2026, 6, 27);
        let end_date = today + chrono::Duration::days(30);
        let obligations = HashMap::new();
        let filings = vec![
            // Overdue & unfiled — MUST be surfaced (the bug fix).
            filing("overdue", date(2026, 6, 1), FilingStatus::Pending),
            // Explicitly flagged overdue — also surfaced.
            filing("overdue2", date(2026, 6, 20), FilingStatus::Overdue),
            // Due within the window — surfaced.
            filing("soon", date(2026, 7, 10), FilingStatus::Pending),
            // Already filed — excluded.
            filing("filed", date(2026, 6, 15), FilingStatus::Filed),
            // Waived — excluded (resolved).
            filing("waived", date(2026, 6, 10), FilingStatus::Waived),
            // Due far in the future, beyond the window — excluded.
            filing("later", date(2026, 9, 1), FilingStatus::Pending),
        ];

        let items = select_due_soon_filings(filings, &entities(), &obligations, today, end_date);
        let ids: Vec<&str> = items.iter().map(|i| i.filing.id.as_str()).collect();

        // Overdue first (sorted by due_date asc), then the in-window upcoming one.
        assert_eq!(ids, vec!["overdue", "overdue2", "soon"]);

        // Overdue items carry negative days_until_due; upcoming is positive.
        assert_eq!(items[0].days_until_due, -26);
        assert!(items[1].days_until_due < 0);
        assert!(items[2].days_until_due > 0);

        // Filed and waived must never appear.
        assert!(!ids.contains(&"filed"));
        assert!(!ids.contains(&"waived"));
        // Beyond-window must not appear.
        assert!(!ids.contains(&"later"));
    }

    #[test]
    fn due_soon_drops_filings_with_unknown_entity() {
        let today = date(2026, 6, 27);
        let end_date = today + chrono::Duration::days(30);
        let mut f = filing("orphan", date(2026, 6, 1), FilingStatus::Pending);
        f.entity_id = "ghost".to_string();
        let items = select_due_soon_filings(vec![f], &entities(), &HashMap::new(), today, end_date);
        assert!(items.is_empty());
    }
}
