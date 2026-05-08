//! Portfolio + Planning endpoints backed by `ff_db::OperationalStore` config_kv.
//!
//! Provides companies, projects, epics, sprints, and task-groups via the same
//! config_kv persistence pattern used by operational work-items. This keeps
//! dashboard PlanningHub and Projects pages functional when ForgeFleet runs in
//! Postgres-backed modes.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use chrono::{NaiveDate, Utc};
use ff_db::OperationalStore;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::epic::{CreateEpic, Epic, EpicStatus, UpdateEpic};
use crate::error::McResult;
use crate::operational_api::{ErrorResponse, McOperationalState, mc_error, to_other_error};
use crate::portfolio::{
    Company, ComplianceSensitivity, CreateCompany, CreateProject, CreateProjectEnvironment,
    CreateProjectRepo, OperatingStage, PortfolioPriority, PortfolioStatus, PortfolioSummary,
    Project, ProjectEnvironment, ProjectRepo, UpdateCompany, UpdateProject,

};
use crate::sprint::{CreateSprint, Sprint, UpdateSprint};
use crate::task_group::{CreateTaskGroup, TaskGroup, UpdateTaskGroup};

const COMPANY_PREFIX: &str = "ff_mc.company.";
const PROJECT_PREFIX: &str = "ff_mc.project.";
const PROJECT_REPO_PREFIX: &str = "ff_mc.project_repo.";
const PROJECT_ENV_PREFIX: &str = "ff_mc.project_env.";
const EPIC_PREFIX: &str = "ff_mc.epic.";
const SPRINT_PREFIX: &str = "ff_mc.sprint.";
const TASK_GROUP_PREFIX: &str = "ff_mc.task_group.";
const STORE_SCAN_LIMIT: u32 = 20_000;

// ─── Helper: generic config_kv CRUD ─────────────────────────────────────────

pub async fn config_kv_get<T: serde::de::DeserializeOwned>(
    store: &OperationalStore,
    key: &str,
) -> McResult<Option<T>> {
    match store.config_get(key).await.map_err(|e| to_other_error("config_get", e))? {
        Some(json) => serde_json::from_str(&json).map_err(|e| to_other_error("deserialize", e)).map(Some),
        None => Ok(None),
    }
}

pub async fn config_kv_set<T: serde::Serialize>(
    store: &OperationalStore,
    key: &str,
    value: &T,
) -> McResult<()> {
    let json = serde_json::to_string(value).map_err(|e| to_other_error("serialize", e))?;
    store.config_set(key, &json).await.map_err(|e| to_other_error("config_set", e))
}

pub async fn config_kv_delete(store: &OperationalStore, key: &str) -> McResult<bool> {
    store.config_delete(key).await.map_err(|e| to_other_error("config_delete", e))
}

pub async fn config_kv_list<T: serde::de::DeserializeOwned>(
    store: &OperationalStore,
    prefix: &str,
) -> McResult<Vec<T>> {
    let entries = store
        .config_list_prefix(prefix, STORE_SCAN_LIMIT)
        .await
        .map_err(|e| to_other_error("config_list_prefix", e))?;
    let mut items = Vec::with_capacity(entries.len());
    for (_key, value) in entries {
        if let Ok(item) = serde_json::from_str::<T>(&value) {
            items.push(item);
        }
    }
    Ok(items)
}

// ─── Company handlers ───────────────────────────────────────────────────────

pub async fn list_companies(State(state): State<std::sync::Arc<McOperationalState>>) -> Json<Vec<Company>> {
    let items = config_kv_list::<Company>(&state.store, COMPANY_PREFIX).await.unwrap_or_default();
    Json(items)
}

pub async fn create_company(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Json(params): Json<CreateCompany>,
) -> Result<Json<Company>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let status = params
        .status
        .as_deref()
        .map(PortfolioStatus::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(PortfolioStatus::Proposed);
    let priority = match params.priority {
        Some(p) => PortfolioPriority::new(p).map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?,
        None => PortfolioPriority::default(),
    };
    let operating_stage = params
        .operating_stage
        .as_deref()
        .map(OperatingStage::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(OperatingStage::Discovery);
    let compliance = params
        .compliance_sensitivity
        .as_deref()
        .map(ComplianceSensitivity::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(ComplianceSensitivity::Low);

    let company = Company {
        id: uuid::Uuid::new_v4().to_string(),
        name: params.name,
        business_unit: params.business_unit,
        status,
        priority,
        owner: params.owner.unwrap_or_else(|| "unassigned".to_string()),
        operating_stage,
        compliance_sensitivity: compliance,
        revenue_model_tags: params.revenue_model_tags,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{COMPANY_PREFIX}{}", company.id), &company)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(company))
}

pub async fn get_company(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Company>, (StatusCode, Json<ErrorResponse>)> {
    match config_kv_get::<Company>(&state.store, &format!("{COMPANY_PREFIX}{id}")).await {
        Ok(Some(c)) => Ok(Json(c)),
        _ => Err(mc_error(StatusCode::NOT_FOUND, format!("company not found: {id}"))),
    }
}

pub async fn update_company(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateCompany>,
) -> Result<Json<Company>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{COMPANY_PREFIX}{id}");
    let mut company = config_kv_get::<Company>(&state.store, &key)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, format!("company not found: {id}")))?;

    if let Some(name) = params.name {
        company.name = name;
    }
    if let Some(bu) = params.business_unit {
        company.business_unit = bu;
    }
    if let Some(status) = params.status {
        company.status = PortfolioStatus::from_str_loose(&status)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(priority) = params.priority {
        company.priority = PortfolioPriority::new(priority)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(owner) = params.owner {
        company.owner = owner;
    }
    if let Some(stage) = params.operating_stage {
        company.operating_stage = OperatingStage::from_str_loose(&stage)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(cs) = params.compliance_sensitivity {
        company.compliance_sensitivity = ComplianceSensitivity::from_str_loose(&cs)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(tags) = params.revenue_model_tags {
        company.revenue_model_tags = tags;
    }
    company.updated_at = Utc::now();

    config_kv_set(&state.store, &key, &company)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(company))
}

pub async fn delete_company(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{COMPANY_PREFIX}{id}");
    config_kv_delete(&state.store, &key)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Project handlers ───────────────────────────────────────────────────────

pub async fn list_projects(State(state): State<std::sync::Arc<McOperationalState>>) -> Json<Vec<Project>> {
    let items = config_kv_list::<Project>(&state.store, PROJECT_PREFIX).await.unwrap_or_default();
    Json(items)
}

pub async fn create_project(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Json(params): Json<CreateProject>,
) -> Result<Json<Project>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let status = params
        .status
        .as_deref()
        .map(PortfolioStatus::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(PortfolioStatus::Proposed);
    let priority = match params.priority {
        Some(p) => PortfolioPriority::new(p).map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?,
        None => PortfolioPriority::default(),
    };
    let operating_stage = params
        .operating_stage
        .as_deref()
        .map(OperatingStage::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(OperatingStage::Discovery);
    let compliance = params
        .compliance_sensitivity
        .as_deref()
        .map(ComplianceSensitivity::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(ComplianceSensitivity::Low);

    let project = Project {
        id: uuid::Uuid::new_v4().to_string(),
        company_id: params.company_id,
        name: params.name,
        description: params.description,
        status,
        priority,
        owner: params.owner.unwrap_or_else(|| "unassigned".to_string()),
        operating_stage,
        compliance_sensitivity: compliance,
        revenue_model_tags: params.revenue_model_tags,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{PROJECT_PREFIX}{}", project.id), &project)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(project))
}

pub async fn get_project(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Project>, (StatusCode, Json<ErrorResponse>)> {
    match config_kv_get::<Project>(&state.store, &format!("{PROJECT_PREFIX}{id}")).await {
        Ok(Some(p)) => Ok(Json(p)),
        _ => Err(mc_error(StatusCode::NOT_FOUND, format!("project not found: {id}"))),
    }
}

pub async fn update_project(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateProject>,
) -> Result<Json<Project>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{PROJECT_PREFIX}{id}");
    let mut project = config_kv_get::<Project>(&state.store, &key)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, format!("project not found: {id}")))?;

    if let Some(name) = params.name {
        project.name = name;
    }
    if let Some(desc) = params.description {
        project.description = desc;
    }
    if let Some(status) = params.status {
        project.status = PortfolioStatus::from_str_loose(&status)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(priority) = params.priority {
        project.priority = PortfolioPriority::new(priority)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(owner) = params.owner {
        project.owner = owner;
    }
    if let Some(stage) = params.operating_stage {
        project.operating_stage = OperatingStage::from_str_loose(&stage)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(cs) = params.compliance_sensitivity {
        project.compliance_sensitivity = ComplianceSensitivity::from_str_loose(&cs)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(tags) = params.revenue_model_tags {
        project.revenue_model_tags = tags;
    }
    project.updated_at = Utc::now();

    config_kv_set(&state.store, &key, &project)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(project))
}

pub async fn delete_project(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    config_kv_delete(&state.store, &format!("{PROJECT_PREFIX}{id}"))
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Project repos ──────────────────────────────────────────────────────────

pub async fn list_project_repos(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(project_id): Path<String>,
) -> Json<Vec<ProjectRepo>> {
    let all = config_kv_list::<ProjectRepo>(&state.store, PROJECT_REPO_PREFIX).await.unwrap_or_default();
    Json(all.into_iter().filter(|r| r.project_id == project_id).collect())
}

pub async fn create_project_repo(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(project_id): Path<String>,
    Json(params): Json<CreateProjectRepo>,
) -> Result<Json<ProjectRepo>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let status = params
        .status
        .as_deref()
        .map(PortfolioStatus::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(PortfolioStatus::Active);

    let repo = ProjectRepo {
        id: uuid::Uuid::new_v4().to_string(),
        project_id,
        repository_url: params.repository_url,
        provider: params.provider.unwrap_or_else(|| "github".to_string()),
        default_branch: params.default_branch.unwrap_or_else(|| "main".to_string()),
        status,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{PROJECT_REPO_PREFIX}{}", repo.id), &repo)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(repo))
}

// ─── Project environments ───────────────────────────────────────────────────

pub async fn list_project_environments(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(project_id): Path<String>,
) -> Json<Vec<ProjectEnvironment>> {
    let all = config_kv_list::<ProjectEnvironment>(&state.store, PROJECT_ENV_PREFIX).await.unwrap_or_default();
    Json(all.into_iter().filter(|e| e.project_id == project_id).collect())
}

pub async fn create_project_environment(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(project_id): Path<String>,
    Json(params): Json<CreateProjectEnvironment>,
) -> Result<Json<ProjectEnvironment>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let status = params
        .status
        .as_deref()
        .map(PortfolioStatus::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(PortfolioStatus::Active);

    let env = ProjectEnvironment {
        id: uuid::Uuid::new_v4().to_string(),
        project_id,
        name: params.name,
        environment_type: params.environment_type.unwrap_or_else(|| "staging".to_string()),
        status,
        owner: params.owner.unwrap_or_else(|| "unassigned".to_string()),
        endpoint_url: params.endpoint_url,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{PROJECT_ENV_PREFIX}{}", env.id), &env)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(env))
}

// ─── Portfolio summary ──────────────────────────────────────────────────────

pub async fn get_portfolio_summary(
    State(state): State<std::sync::Arc<McOperationalState>>,
) -> Json<PortfolioSummary> {
    let companies = config_kv_list::<crate::portfolio::Company>(&state.store, COMPANY_PREFIX).await.unwrap_or_default();
    let projects = config_kv_list::<Project>(&state.store, PROJECT_PREFIX).await.unwrap_or_default();

    let active_projects = projects.iter().filter(|p| matches!(p.status, PortfolioStatus::Active)).count() as i64;
    let mut projects_by_status = HashMap::new();
    let mut projects_by_operating_stage = HashMap::new();
    let mut projects_by_compliance_sensitivity = HashMap::new();
    let mut projects_by_owner = HashMap::new();
    let mut revenue_model_tag_counts = HashMap::new();

    for p in &projects {
        *projects_by_status.entry(p.status.as_str().to_string()).or_insert(0i64) += 1;
        *projects_by_operating_stage.entry(p.operating_stage.as_str().to_string()).or_insert(0i64) += 1;
        *projects_by_compliance_sensitivity.entry(p.compliance_sensitivity.as_str().to_string()).or_insert(0i64) += 1;
        *projects_by_owner.entry(p.owner.clone()).or_insert(0i64) += 1;
        for tag in &p.revenue_model_tags {
            *revenue_model_tag_counts.entry(tag.clone()).or_insert(0i64) += 1;
        }
    }

    Json(PortfolioSummary {
        total_companies: companies.len() as i64,
        total_projects: projects.len() as i64,
        active_projects,
        projects_by_status,
        projects_by_operating_stage,
        projects_by_compliance_sensitivity,
        projects_by_owner,
        revenue_model_tag_counts,
    })
}

// ─── Epic handlers ──────────────────────────────────────────────────────────

pub async fn list_epics(State(state): State<std::sync::Arc<McOperationalState>>) -> Json<Vec<Epic>> {
    let items = config_kv_list::<Epic>(&state.store, EPIC_PREFIX).await.unwrap_or_default();
    Json(items)
}

pub async fn create_epic(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Json(params): Json<CreateEpic>,
) -> Result<Json<Epic>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let status = params
        .status
        .as_deref()
        .map(EpicStatus::from_str_loose)
        .transpose()
        .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or(EpicStatus::Open);

    let epic = Epic {
        id: uuid::Uuid::new_v4().to_string(),
        title: params.title,
        description: params.description,
        status,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{EPIC_PREFIX}{}", epic.id), &epic)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(epic))
}

pub async fn get_epic(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Epic>, (StatusCode, Json<ErrorResponse>)> {
    match config_kv_get::<Epic>(&state.store, &format!("{EPIC_PREFIX}{id}")).await {
        Ok(Some(e)) => Ok(Json(e)),
        _ => Err(mc_error(StatusCode::NOT_FOUND, format!("epic not found: {id}"))),
    }
}

pub async fn update_epic(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateEpic>,
) -> Result<Json<Epic>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{EPIC_PREFIX}{id}");
    let mut epic = config_kv_get::<Epic>(&state.store, &key)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, format!("epic not found: {id}")))?;

    if let Some(title) = params.title {
        epic.title = title;
    }
    if let Some(desc) = params.description {
        epic.description = desc;
    }
    if let Some(status) = params.status {
        epic.status = EpicStatus::from_str_loose(&status)
            .map_err(|e| mc_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    epic.updated_at = Utc::now();

    config_kv_set(&state.store, &key, &epic)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(epic))
}

pub async fn delete_epic(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    config_kv_delete(&state.store, &format!("{EPIC_PREFIX}{id}"))
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_epic_progress(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    // Progress is computed from work-items linked to this epic.
    // Operational store keeps work-items with optional epic_id field.
    let work_items = config_kv_list::<crate::work_item::WorkItem>(&state.store, crate::operational_api::WORK_ITEM_KEY_PREFIX)
        .await
        .unwrap_or_default();
    let epic_items: Vec<_> = work_items.into_iter().filter(|wi| wi.epic_id.as_deref() == Some(&id)).collect();
    let total = epic_items.len();
    let done = epic_items.iter().filter(|wi| matches!(wi.status, crate::work_item::WorkItemStatus::Done)).count();
    let pct = if total > 0 { (done as f64 / total as f64) * 100.0 } else { 0.0 };

    Ok(Json(json!({
        "epic_id": id,
        "total_items": total,
        "done_items": done,
        "progress_pct": pct,
        "work_item_ids": epic_items.iter().map(|wi| wi.id.clone()).collect::<Vec<_>>(),
    })))
}

// ─── Sprint handlers ────────────────────────────────────────────────────────

pub async fn list_sprints(State(state): State<std::sync::Arc<McOperationalState>>) -> Json<Vec<Sprint>> {
    let items = config_kv_list::<Sprint>(&state.store, SPRINT_PREFIX).await.unwrap_or_default();
    Json(items)
}

pub async fn create_sprint(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Json(params): Json<CreateSprint>,
) -> Result<Json<Sprint>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let start_date = params.start_date.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
    let end_date = params.end_date.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

    let sprint = Sprint {
        id: uuid::Uuid::new_v4().to_string(),
        name: params.name,
        start_date,
        end_date,
        goal: params.goal,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{SPRINT_PREFIX}{}", sprint.id), &sprint)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(sprint))
}

pub async fn get_sprint(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Sprint>, (StatusCode, Json<ErrorResponse>)> {
    match config_kv_get::<Sprint>(&state.store, &format!("{SPRINT_PREFIX}{id}")).await {
        Ok(Some(s)) => Ok(Json(s)),
        _ => Err(mc_error(StatusCode::NOT_FOUND, format!("sprint not found: {id}"))),
    }
}

pub async fn update_sprint(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateSprint>,
) -> Result<Json<Sprint>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{SPRINT_PREFIX}{id}");
    let mut sprint = config_kv_get::<Sprint>(&state.store, &key)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, format!("sprint not found: {id}")))?;

    if let Some(name) = params.name {
        sprint.name = name;
    }
    if let Some(start) = params.start_date {
        sprint.start_date = start.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
    }
    if let Some(end) = params.end_date {
        sprint.end_date = end.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
    }
    if let Some(goal) = params.goal {
        sprint.goal = goal;
    }
    sprint.updated_at = Utc::now();

    config_kv_set(&state.store, &key, &sprint)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(sprint))
}

pub async fn delete_sprint(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    config_kv_delete(&state.store, &format!("{SPRINT_PREFIX}{id}"))
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_sprint_stats(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let work_items = config_kv_list::<crate::work_item::WorkItem>(&state.store, crate::operational_api::WORK_ITEM_KEY_PREFIX)
        .await
        .unwrap_or_default();
    let sprint_items: Vec<_> = work_items.into_iter().filter(|wi| wi.sprint_id.as_deref() == Some(&id)).collect();
    let total = sprint_items.len();
    let done = sprint_items.iter().filter(|wi| matches!(wi.status, crate::work_item::WorkItemStatus::Done)).count();
    let in_progress = sprint_items.iter().filter(|wi| matches!(wi.status, crate::work_item::WorkItemStatus::InProgress)).count();
    let blocked = sprint_items.iter().filter(|wi| matches!(wi.status, crate::work_item::WorkItemStatus::Blocked)).count();
    let velocity = if total > 0 { done as f64 } else { 0.0 };

    Ok(Json(json!({
        "sprint_id": id,
        "total_items": total,
        "done_items": done,
        "in_progress_items": in_progress,
        "blocked_items": blocked,
        "velocity": velocity,
    })))
}

pub async fn get_sprint_burndown(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    // Minimal burndown: current total vs done.
    let work_items = config_kv_list::<crate::work_item::WorkItem>(&state.store, crate::operational_api::WORK_ITEM_KEY_PREFIX)
        .await
        .unwrap_or_default();
    let sprint_items: Vec<_> = work_items.into_iter().filter(|wi| wi.sprint_id.as_deref() == Some(&id)).collect();
    let total = sprint_items.len();
    let done = sprint_items.iter().filter(|wi| matches!(wi.status, crate::work_item::WorkItemStatus::Done)).count();
    let remaining = total.saturating_sub(done);

    Ok(Json(json!({
        "sprint_id": id,
        "total": total,
        "done": done,
        "remaining": remaining,
        "points": [{"day": 0, "remaining": total}, {"day": 1, "remaining": remaining}],
    })))
}

// ─── Task group handlers ────────────────────────────────────────────────────

pub async fn list_task_groups(State(state): State<std::sync::Arc<McOperationalState>>) -> Json<Vec<TaskGroup>> {
    let items = config_kv_list::<TaskGroup>(&state.store, TASK_GROUP_PREFIX).await.unwrap_or_default();
    Json(items)
}

pub async fn create_task_group(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Json(params): Json<CreateTaskGroup>,
) -> Result<Json<TaskGroup>, (StatusCode, Json<ErrorResponse>)> {
    let now = Utc::now();
    let tg = TaskGroup {
        id: uuid::Uuid::new_v4().to_string(),
        name: params.name,
        description: params.description,
        created_at: now,
        updated_at: now,
    };

    config_kv_set(&state.store, &format!("{TASK_GROUP_PREFIX}{}", tg.id), &tg)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(tg))
}

pub async fn get_task_group(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskGroup>, (StatusCode, Json<ErrorResponse>)> {
    match config_kv_get::<TaskGroup>(&state.store, &format!("{TASK_GROUP_PREFIX}{id}")).await {
        Ok(Some(tg)) => Ok(Json(tg)),
        _ => Err(mc_error(StatusCode::NOT_FOUND, format!("task group not found: {id}"))),
    }
}

pub async fn update_task_group(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
    Json(params): Json<UpdateTaskGroup>,
) -> Result<Json<TaskGroup>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{TASK_GROUP_PREFIX}{id}");
    let mut tg = config_kv_get::<TaskGroup>(&state.store, &key)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, format!("task group not found: {id}")))?;

    if let Some(name) = params.name {
        tg.name = name;
    }
    if let Some(desc) = params.description {
        tg.description = desc;
    }
    tg.updated_at = Utc::now();

    config_kv_set(&state.store, &key, &tg)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(tg))
}

pub async fn delete_task_group(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    config_kv_delete(&state.store, &format!("{TASK_GROUP_PREFIX}{id}"))
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_task_group_items(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path(id): Path<String>,
) -> Json<Vec<crate::work_item::WorkItem>> {
    let work_items = config_kv_list::<crate::work_item::WorkItem>(&state.store, crate::operational_api::WORK_ITEM_KEY_PREFIX)
        .await
        .unwrap_or_default();
    Json(work_items.into_iter().filter(|wi| wi.task_group_id.as_deref() == Some(&id)).collect())
}

#[derive(Debug, Deserialize, Default)]
pub struct AssignTaskGroupItemRequest {
    sequence_order: Option<i32>,
}

pub async fn assign_task_group_item(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path((_id, work_item_id)): Path<(String, String)>,
    body: Option<Json<AssignTaskGroupItemRequest>>,
) -> Result<Json<crate::work_item::WorkItem>, (StatusCode, Json<crate::operational_api::ErrorResponse>)> {
    let sequence_order = body.and_then(|Json(v)| v.sequence_order);
    let prefix = crate::operational_api::WORK_ITEM_KEY_PREFIX;
    let mut work_items = config_kv_list::<crate::work_item::WorkItem>(&state.store, prefix)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let item = work_items.iter_mut().find(|wi| wi.id == work_item_id)
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, "work item not found"))?;
    item.task_group_id = Some(_id);
    item.sequence_order = sequence_order;
    item.updated_at = Utc::now();
    config_kv_set(&state.store, &format!("{prefix}{}", item.id), item)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(item.clone()))
}

pub async fn unassign_task_group_item(
    State(state): State<std::sync::Arc<McOperationalState>>,
    Path((_id, work_item_id)): Path<(String, String)>,
) -> Result<Json<crate::work_item::WorkItem>, (StatusCode, Json<crate::operational_api::ErrorResponse>)> {
    let prefix = crate::operational_api::WORK_ITEM_KEY_PREFIX;
    let mut work_items = config_kv_list::<crate::work_item::WorkItem>(&state.store, prefix)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let item = work_items.iter_mut().find(|wi| wi.id == work_item_id)
        .ok_or_else(|| mc_error(StatusCode::NOT_FOUND, "work item not found"))?;
    if item.task_group_id.as_deref() != Some(&_id) {
        return Err(mc_error(StatusCode::BAD_REQUEST, "work item is not assigned to this task group"));
    }
    item.task_group_id = None;
    item.sequence_order = None;
    item.updated_at = Utc::now();
    config_kv_set(&state.store, &format!("{prefix}{}", item.id), item)
        .await
        .map_err(|e| mc_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(item.clone()))
}

// ─── Router assembly ────────────────────────────────────────────────────────

pub fn portfolio_router(state: std::sync::Arc<McOperationalState>) -> Router {
    Router::new()
        // Companies
        .route("/api/mc/companies", get(list_companies).post(create_company))
        .route("/api/mc/companies/{id}", get(get_company).patch(update_company).delete(delete_company))
        // Projects
        .route("/api/mc/projects", get(list_projects).post(create_project))
        .route("/api/mc/projects/{id}", get(get_project).patch(update_project).delete(delete_project))
        .route("/api/mc/projects/{id}/repos", get(list_project_repos).post(create_project_repo))
        .route("/api/mc/projects/{id}/environments", get(list_project_environments).post(create_project_environment))
        // Portfolio summary
        .route("/api/mc/portfolio/summary", get(get_portfolio_summary))
        // Epics
        .route("/api/mc/epics", get(list_epics).post(create_epic))
        .route("/api/mc/epics/{id}", get(get_epic).patch(update_epic).delete(delete_epic))
        .route("/api/mc/epics/{id}/progress", get(get_epic_progress))
        // Sprints
        .route("/api/mc/sprints", get(list_sprints).post(create_sprint))
        .route("/api/mc/sprints/{id}", get(get_sprint).patch(update_sprint).delete(delete_sprint))
        .route("/api/mc/sprints/{id}/stats", get(get_sprint_stats))
        .route("/api/mc/sprints/{id}/burndown", get(get_sprint_burndown))
        // Task groups
        .route("/api/mc/task-groups", get(list_task_groups).post(create_task_group))
        .route("/api/mc/task-groups/{id}", get(get_task_group).patch(update_task_group).delete(delete_task_group))
        .route("/api/mc/task-groups/{id}/items", get(list_task_group_items))
        .with_state(state)
}
