//! Axum REST API routes for Mission Control.
//!
//! Provides endpoints for work items, epics, sprints, board view, and dashboard.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};

use crate::auto_link::{self, AutoLinkConfig};
use crate::board::{BoardFilter, BoardView};
use crate::dashboard::DashboardStats;
use crate::db::McDb;
use crate::dependency::WorkItemDependency;
use crate::epic::{CreateEpic, Epic, UpdateEpic};
use crate::legal::{
    ComplianceObligation, CreateComplianceObligation, CreateFiling, CreateLegalEntity, Filing,
    FilingFilter, FilingStatus, LegalEntity, UpdateComplianceObligation, UpdateFiling,
    UpdateLegalEntity,
};
use crate::portfolio::{
    Company, CompanyFilter, ComplianceSensitivity, CreateCompany, CreateProject,
    CreateProjectEnvironment, CreateProjectRepo, OperatingStage, PortfolioStatus, PortfolioSummary,
    Project, ProjectEnvironment, ProjectFilter, ProjectRepo, UpdateCompany, UpdateProject,
    UpdateProjectEnvironment, UpdateProjectRepo,
};
use crate::review_item::{CreateReviewItem, ReviewItem, UpdateReviewItem};
use crate::sprint::{CreateSprint, Sprint, UpdateSprint};
use crate::task_group::{CreateTaskGroup, TaskGroup, UpdateTaskGroup};
use crate::work_item::{CreateWorkItem, UpdateWorkItem, WorkItem, WorkItemFilter, WorkItemStatus};

/// Shared application state for all MC API routes.
#[derive(Debug, Clone)]
pub struct McState {
    pub db: McDb,
}

/// Build the full Mission Control API router.
pub fn mc_router(db: McDb) -> Router {
    let state = Arc::new(McState { db });

    Router::new()
        // Work items
        .route("/api/mc/work-items", get(list_work_items))
        .route("/api/mc/work-items", post(create_work_item))
        .route("/api/mc/work-items/{id}", get(get_work_item))
        .route("/api/mc/work-items/{id}", patch(update_work_item))
        .route("/api/mc/work-items/{id}", delete(delete_work_item))
        .route(
            "/api/mc/work-items/{id}/links",
            get(suggest_work_item_links),
        )
        .route("/api/mc/work-items/{id}/claim", post(claim_work_item))
        .route("/api/mc/work-items/{id}/complete", post(complete_work_item))
        .route("/api/mc/work-items/{id}/fail", post(fail_work_item))
        .route("/api/mc/work-items/{id}/escalate", post(escalate_work_item))
        // Review workflow / checklist
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
        // Dependency workflow
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
        // Task groups
        .route("/api/mc/task-groups", get(list_task_groups))
        .route("/api/mc/task-groups", post(create_task_group))
        .route("/api/mc/task-groups/{id}", get(get_task_group))
        .route("/api/mc/task-groups/{id}", patch(update_task_group))
        .route("/api/mc/task-groups/{id}", delete(delete_task_group))
        .route("/api/mc/task-groups/{id}/items", get(list_task_group_items))
        .route(
            "/api/mc/task-groups/{id}/items/{work_item_id}",
            post(assign_task_group_item),
        )
        .route(
            "/api/mc/task-groups/{id}/items/{work_item_id}",
            delete(unassign_task_group_item),
        )
        // Legal entities
        .route("/api/mc/legal/entities", get(list_legal_entities))
        .route("/api/mc/legal/entities", post(create_legal_entity))
        .route("/api/mc/legal/entities/{id}", get(get_legal_entity))
        .route("/api/mc/legal/entities/{id}", patch(update_legal_entity))
        .route("/api/mc/legal/entities/{id}", delete(delete_legal_entity))
        // Compliance obligations
        .route("/api/mc/legal/obligations", get(list_obligations))
        .route("/api/mc/legal/obligations", post(create_obligation))
        .route("/api/mc/legal/obligations/{id}", get(get_obligation))
        .route("/api/mc/legal/obligations/{id}", patch(update_obligation))
        .route("/api/mc/legal/obligations/{id}", delete(delete_obligation))
        // Filings
        .route("/api/mc/legal/filings/due-soon", get(list_due_soon_filings))
        .route("/api/mc/legal/filings", get(list_filings))
        .route("/api/mc/legal/filings", post(create_filing))
        .route("/api/mc/legal/filings/{id}", get(get_filing))
        .route("/api/mc/legal/filings/{id}", patch(update_filing))
        .route("/api/mc/legal/filings/{id}", delete(delete_filing))
        // Portfolio: companies
        .route("/api/mc/companies", get(list_companies))
        .route("/api/mc/companies", post(create_company))
        .route("/api/mc/companies/{id}", get(get_company))
        .route("/api/mc/companies/{id}", patch(update_company))
        .route("/api/mc/companies/{id}", delete(delete_company))
        // Portfolio: projects
        .route("/api/mc/projects", get(list_projects))
        .route("/api/mc/projects", post(create_project))
        .route("/api/mc/projects/{id}", get(get_project))
        .route("/api/mc/projects/{id}", patch(update_project))
        .route("/api/mc/projects/{id}", delete(delete_project))
        // Portfolio: project repos
        .route("/api/mc/projects/{id}/repos", get(list_project_repos))
        .route("/api/mc/projects/{id}/repos", post(create_project_repo))
        .route("/api/mc/project-repos/{id}", get(get_project_repo))
        .route("/api/mc/project-repos/{id}", patch(update_project_repo))
        .route("/api/mc/project-repos/{id}", delete(delete_project_repo))
        // Portfolio: project environments
        .route(
            "/api/mc/projects/{id}/environments",
            get(list_project_environments),
        )
        .route(
            "/api/mc/projects/{id}/environments",
            post(create_project_environment),
        )
        .route(
            "/api/mc/project-environments/{id}",
            get(get_project_environment),
        )
        .route(
            "/api/mc/project-environments/{id}",
            patch(update_project_environment),
        )
        .route(
            "/api/mc/project-environments/{id}",
            delete(delete_project_environment),
        )
        // Portfolio summary
        .route("/api/mc/portfolio/summary", get(get_portfolio_summary))
        // Epics
        .route("/api/mc/epics", get(list_epics))
        .route("/api/mc/epics", post(create_epic))
        .route("/api/mc/epics/{id}", get(get_epic))
        .route("/api/mc/epics/{id}", patch(update_epic))
        .route("/api/mc/epics/{id}", delete(delete_epic))
        .route("/api/mc/epics/{id}/progress", get(get_epic_progress))
        // Sprints
        .route("/api/mc/sprints", get(list_sprints))
        .route("/api/mc/sprints", post(create_sprint))
        .route("/api/mc/sprints/{id}", get(get_sprint))
        .route("/api/mc/sprints/{id}", patch(update_sprint))
        .route("/api/mc/sprints/{id}", delete(delete_sprint))
        .route("/api/mc/sprints/{id}/stats", get(get_sprint_stats))
        .route("/api/mc/sprints/{id}/burndown", get(get_sprint_burndown))
        // Board
        .route("/api/mc/board", get(get_board))
        // Dashboard
        .route("/api/mc/dashboard", get(get_dashboard))
        .with_state(state)
}

// ─── Error Response ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

fn mc_error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

fn handle_mc_err(e: crate::error::McError) -> impl IntoResponse {
    use crate::error::McError;
    match &e {
        McError::WorkItemNotFound { .. }
        | McError::EpicNotFound { .. }
        | McError::SprintNotFound { .. }
        | McError::ReviewItemNotFound { .. }
        | McError::TaskGroupNotFound { .. }
        | McError::CompanyNotFound { .. }
        | McError::ProjectNotFound { .. }
        | McError::ProjectRepoNotFound { .. }
        | McError::ProjectEnvironmentNotFound { .. }
        | McError::LegalEntityNotFound { .. }
        | McError::ComplianceObligationNotFound { .. }
        | McError::FilingNotFound { .. } => mc_error(StatusCode::NOT_FOUND, e.to_string()),
        McError::InvalidStatus { .. }
        | McError::InvalidPriority { .. }
        | McError::InvalidOperatingStage { .. }
        | McError::InvalidComplianceSensitivity { .. } => {
            mc_error(StatusCode::BAD_REQUEST, e.to_string())
        }
        _ => mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ─── Work Item Endpoints ─────────────────────────────────────────────────────

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
    State(state): State<Arc<McState>>,
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

    match WorkItem::list(&state.db, &filter) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_work_item(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateWorkItem>,
) -> impl IntoResponse {
    match WorkItem::create(&state.db, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItem::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateWorkItem>,
) -> impl IntoResponse {
    match WorkItem::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItem::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn suggest_work_item_links(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let item = match WorkItem::get(&state.db, &id) {
        Ok(i) => i,
        Err(e) => return handle_mc_err(e).into_response(),
    };

    match auto_link::suggest_links(
        &state.db,
        &id,
        &item.description,
        &item.title,
        &AutoLinkConfig::default(),
    ) {
        Ok(links) => Json(links).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ClaimWorkItemRequest {
    assignee: Option<String>,
}

async fn claim_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    body: Option<Json<ClaimWorkItemRequest>>,
) -> impl IntoResponse {
    let assignee = body.and_then(|Json(v)| v.assignee);
    match WorkItem::claim(&state.db, &id, assignee) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn complete_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItem::complete(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn fail_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItem::fail(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn escalate_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItem::escalate(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Review Endpoints ────────────────────────────────────────────────────────

async fn submit_review(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ReviewItem::submit_review(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn start_review(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ReviewItem::start_review(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn complete_review(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ReviewItem::complete_review(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn list_review_items_for_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ReviewItem::list_for_work_item(&state.db, &id) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_review_item_for_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<CreateReviewItem>,
) -> impl IntoResponse {
    match ReviewItem::create(&state.db, &id, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_review_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateReviewItem>,
) -> impl IntoResponse {
    match ReviewItem::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_review_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ReviewItem::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn reset_review_items_for_work_item(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ReviewItem::reset_for_work_item(&state.db, &id) {
        Ok(updated) => Json(serde_json::json!({"updated": updated})).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Dependency Endpoints ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AddDependencyRequest {
    depends_on_id: String,
}

async fn list_work_item_dependencies(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItemDependency::list_for_work_item(&state.db, &id) {
        Ok(deps) => Json(deps).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn add_work_item_dependency(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(body): Json<AddDependencyRequest>,
) -> impl IntoResponse {
    match WorkItemDependency::add(&state.db, &id, &body.depends_on_id) {
        Ok(dep) => (StatusCode::CREATED, Json(dep)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn remove_work_item_dependency(
    State(state): State<Arc<McState>>,
    Path((id, depends_on_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match WorkItemDependency::remove(&state.db, &id, &depends_on_id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => mc_error(StatusCode::NOT_FOUND, "dependency not found").into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn check_work_item_dependencies(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match WorkItemDependency::check(&state.db, &id) {
        Ok(result) => Json(result).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Task Group Endpoints ───────────────────────────────────────────────────

async fn list_task_groups(State(state): State<Arc<McState>>) -> impl IntoResponse {
    match TaskGroup::list(&state.db) {
        Ok(groups) => Json(groups).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_task_group(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateTaskGroup>,
) -> impl IntoResponse {
    match TaskGroup::create(&state.db, params) {
        Ok(group) => (StatusCode::CREATED, Json(group)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_task_group(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match TaskGroup::get(&state.db, &id) {
        Ok(group) => Json(group).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_task_group(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateTaskGroup>,
) -> impl IntoResponse {
    match TaskGroup::update(&state.db, &id, params) {
        Ok(group) => Json(group).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_task_group(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match TaskGroup::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn list_task_group_items(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match TaskGroup::list_items(&state.db, &id) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
struct AssignTaskGroupItemRequest {
    sequence_order: Option<i32>,
}

async fn assign_task_group_item(
    State(state): State<Arc<McState>>,
    Path((id, work_item_id)): Path<(String, String)>,
    body: Option<Json<AssignTaskGroupItemRequest>>,
) -> impl IntoResponse {
    let sequence_order = body.and_then(|Json(v)| v.sequence_order);
    match TaskGroup::assign_work_item(&state.db, &id, &work_item_id, sequence_order) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn unassign_task_group_item(
    State(state): State<Arc<McState>>,
    Path((id, work_item_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match TaskGroup::unassign_work_item(&state.db, &id, &work_item_id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Portfolio Endpoints ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CompanyQuery {
    status: Option<String>,
    owner: Option<String>,
    operating_stage: Option<String>,
    compliance_sensitivity: Option<String>,
    business_unit: Option<String>,
}

async fn list_companies(
    State(state): State<Arc<McState>>,
    Query(q): Query<CompanyQuery>,
) -> impl IntoResponse {
    let status = match q.status {
        Some(raw) => match PortfolioStatus::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let operating_stage = match q.operating_stage {
        Some(raw) => match OperatingStage::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let compliance_sensitivity = match q.compliance_sensitivity {
        Some(raw) => match ComplianceSensitivity::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let filter = CompanyFilter {
        status,
        owner: q.owner,
        operating_stage,
        compliance_sensitivity,
        business_unit: q.business_unit,
    };

    match Company::list(&state.db, &filter) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_company(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateCompany>,
) -> impl IntoResponse {
    match Company::create(&state.db, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_company(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Company::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_company(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateCompany>,
) -> impl IntoResponse {
    match Company::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_company(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Company::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ProjectQuery {
    company_id: Option<String>,
    status: Option<String>,
    owner: Option<String>,
    operating_stage: Option<String>,
    compliance_sensitivity: Option<String>,
}

async fn list_projects(
    State(state): State<Arc<McState>>,
    Query(q): Query<ProjectQuery>,
) -> impl IntoResponse {
    let status = match q.status {
        Some(raw) => match PortfolioStatus::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let operating_stage = match q.operating_stage {
        Some(raw) => match OperatingStage::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let compliance_sensitivity = match q.compliance_sensitivity {
        Some(raw) => match ComplianceSensitivity::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let filter = ProjectFilter {
        company_id: q.company_id,
        status,
        owner: q.owner,
        operating_stage,
        compliance_sensitivity,
    };

    match Project::list(&state.db, &filter) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_project(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateProject>,
) -> impl IntoResponse {
    match Project::create(&state.db, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_project(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Project::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_project(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateProject>,
) -> impl IntoResponse {
    match Project::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_project(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Project::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn list_project_repos(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ProjectRepo::list_for_project(&state.db, &id) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_project_repo(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<CreateProjectRepo>,
) -> impl IntoResponse {
    match ProjectRepo::create(&state.db, &id, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_project_repo(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ProjectRepo::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_project_repo(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateProjectRepo>,
) -> impl IntoResponse {
    match ProjectRepo::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_project_repo(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ProjectRepo::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn list_project_environments(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ProjectEnvironment::list_for_project(&state.db, &id) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_project_environment(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<CreateProjectEnvironment>,
) -> impl IntoResponse {
    match ProjectEnvironment::create(&state.db, &id, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_project_environment(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ProjectEnvironment::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_project_environment(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateProjectEnvironment>,
) -> impl IntoResponse {
    match ProjectEnvironment::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_project_environment(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ProjectEnvironment::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_portfolio_summary(State(state): State<Arc<McState>>) -> impl IntoResponse {
    match PortfolioSummary::compute(&state.db) {
        Ok(summary) => Json(summary).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Legal / Compliance Endpoints ───────────────────────────────────────────

async fn list_legal_entities(State(state): State<Arc<McState>>) -> impl IntoResponse {
    match LegalEntity::list(&state.db) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_legal_entity(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateLegalEntity>,
) -> impl IntoResponse {
    match LegalEntity::create(&state.db, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_legal_entity(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match LegalEntity::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_legal_entity(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateLegalEntity>,
) -> impl IntoResponse {
    match LegalEntity::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_legal_entity(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match LegalEntity::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ObligationQuery {
    entity_id: Option<String>,
}

async fn list_obligations(
    State(state): State<Arc<McState>>,
    Query(q): Query<ObligationQuery>,
) -> impl IntoResponse {
    match ComplianceObligation::list(&state.db, q.entity_id.as_deref()) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_obligation(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateComplianceObligation>,
) -> impl IntoResponse {
    match ComplianceObligation::create(&state.db, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_obligation(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ComplianceObligation::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_obligation(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateComplianceObligation>,
) -> impl IntoResponse {
    match ComplianceObligation::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_obligation(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match ComplianceObligation::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct FilingQueryParams {
    entity_id: Option<String>,
    obligation_id: Option<String>,
    status: Option<String>,
}

async fn list_filings(
    State(state): State<Arc<McState>>,
    Query(q): Query<FilingQueryParams>,
) -> impl IntoResponse {
    let status = match q.status {
        Some(raw) => match FilingStatus::from_str_loose(&raw) {
            Ok(value) => Some(value),
            Err(e) => return handle_mc_err(e).into_response(),
        },
        None => None,
    };

    let filter = FilingFilter {
        entity_id: q.entity_id,
        obligation_id: q.obligation_id,
        status,
    };

    match Filing::list(&state.db, &filter) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_filing(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateFiling>,
) -> impl IntoResponse {
    match Filing::create(&state.db, params) {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_filing(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Filing::get(&state.db, &id) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_filing(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateFiling>,
) -> impl IntoResponse {
    match Filing::update(&state.db, &id, params) {
        Ok(item) => Json(item).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_filing(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Filing::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct DueSoonQuery {
    days: Option<i64>,
}

async fn list_due_soon_filings(
    State(state): State<Arc<McState>>,
    Query(q): Query<DueSoonQuery>,
) -> impl IntoResponse {
    let days = q.days.unwrap_or(30).clamp(0, 365);
    match Filing::due_within_days(&state.db, days) {
        Ok(items) => Json(items).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Epic Endpoints ──────────────────────────────────────────────────────────

async fn list_epics(State(state): State<Arc<McState>>) -> impl IntoResponse {
    match Epic::list(&state.db) {
        Ok(epics) => Json(epics).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_epic(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateEpic>,
) -> impl IntoResponse {
    match Epic::create(&state.db, params) {
        Ok(epic) => (StatusCode::CREATED, Json(epic)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_epic(State(state): State<Arc<McState>>, Path(id): Path<String>) -> impl IntoResponse {
    match Epic::get(&state.db, &id) {
        Ok(epic) => Json(epic).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_epic(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateEpic>,
) -> impl IntoResponse {
    match Epic::update(&state.db, &id, params) {
        Ok(epic) => Json(epic).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_epic(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Epic::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_epic_progress(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Epic::with_progress(&state.db, &id) {
        Ok(prog) => Json(prog).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Sprint Endpoints ────────────────────────────────────────────────────────

async fn list_sprints(State(state): State<Arc<McState>>) -> impl IntoResponse {
    match Sprint::list(&state.db) {
        Ok(sprints) => Json(sprints).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn create_sprint(
    State(state): State<Arc<McState>>,
    Json(params): Json<CreateSprint>,
) -> impl IntoResponse {
    match Sprint::create(&state.db, params) {
        Ok(sprint) => (StatusCode::CREATED, Json(sprint)).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_sprint(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Sprint::get(&state.db, &id) {
        Ok(sprint) => Json(sprint).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn update_sprint(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateSprint>,
) -> impl IntoResponse {
    match Sprint::update(&state.db, &id, params) {
        Ok(sprint) => Json(sprint).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn delete_sprint(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Sprint::delete(&state.db, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_sprint_stats(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Sprint::with_stats(&state.db, &id) {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

async fn get_sprint_burndown(
    State(state): State<Arc<McState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match Sprint::burndown(&state.db, &id) {
        Ok(data) => Json(data).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Board Endpoint ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BoardQuery {
    assignee: Option<String>,
    epic_id: Option<String>,
    sprint_id: Option<String>,
    task_group_id: Option<String>,
    label: Option<String>,
}

async fn get_board(
    State(state): State<Arc<McState>>,
    Query(q): Query<BoardQuery>,
) -> impl IntoResponse {
    let filter = BoardFilter {
        assignee: q.assignee,
        epic_id: q.epic_id,
        sprint_id: q.sprint_id,
        task_group_id: q.task_group_id,
        label: q.label,
    };

    match BoardView::build(&state.db, &filter) {
        Ok(board) => Json(board).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}

// ─── Dashboard Endpoint ──────────────────────────────────────────────────────

async fn get_dashboard(State(state): State<Arc<McState>>) -> impl IntoResponse {
    match DashboardStats::compute(&state.db) {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => handle_mc_err(e).into_response(),
    }
}
