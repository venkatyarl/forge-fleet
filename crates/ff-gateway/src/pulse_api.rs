//! HTTP API endpoints backed by the Pulse v2 schema (V14–V17).
//!
//! Mounted in server.rs under `/api/fleet/*`, `/api/software/*`,
//! `/api/projects/*`, `/api/pm/*`, `/api/alerts/*`, `/api/metrics/*`,
//! `/api/ha/*`, `/api/docker/*`, and `/api/llm/servers`.
//!
//! All endpoints query the Postgres `operational_store` directly via `sqlx`.
//! They return shapes tuned for the dashboard panels — no attempt to be a
//! general-purpose REST API.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use tokio_stream::wrappers::ReceiverStream;

use crate::server::GatewayState;

// ─── helpers ────────────────────────────────────────────────────────────

fn pool_from_state(state: &GatewayState) -> Result<&ff_db::PgPool, (StatusCode, Json<Value>)> {
    state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "postgres pool not available"})),
            )
        })
}

fn db_err(op: &str, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!("pulse api error ({op}): {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("{op}: {e}")})),
    )
}

fn iso(ts: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    ts.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

// ─── /api/fleet/computers ───────────────────────────────────────────────

pub async fn list_computers(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT
            c.id,
            c.name,
            c.primary_ip,
            c.hostname,
            c.os_family,
            c.os_distribution,
            c.os_version,
            c.cpu_cores,
            c.total_ram_gb,
            c.total_disk_gb,
            c.has_gpu,
            c.gpu_kind,
            c.gpu_count,
            c.gpu_model,
            c.gpu_total_vram_gb,
            c.status,
            c.enrolled_at,
            c.last_seen_at,
            c.offline_since,
            c.status_changed_at,
            fm.role AS member_role,
            fm.runtime AS member_runtime,
            fm.election_priority,
            (
                SELECT COUNT(*) FROM computer_model_deployments d
                WHERE d.computer_id = c.id AND d.status = 'active'
            ) AS active_deployment_count,
            (
                SELECT m.cpu_pct FROM computer_metrics_history m
                WHERE m.computer_id = c.id
                ORDER BY m.recorded_at DESC LIMIT 1
            ) AS latest_cpu_pct,
            (
                SELECT m.ram_pct FROM computer_metrics_history m
                WHERE m.computer_id = c.id
                ORDER BY m.recorded_at DESC LIMIT 1
            ) AS latest_ram_pct,
            (
                SELECT m.disk_free_gb FROM computer_metrics_history m
                WHERE m.computer_id = c.id
                ORDER BY m.recorded_at DESC LIMIT 1
            ) AS latest_disk_free_gb,
            (
                SELECT m.recorded_at FROM computer_metrics_history m
                WHERE m.computer_id = c.id
                ORDER BY m.recorded_at DESC LIMIT 1
            ) AS latest_recorded_at
        FROM computers c
        LEFT JOIN fleet_members fm ON fm.computer_id = c.id
        ORDER BY c.name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("list_computers", e))?;

    let computers: Vec<Value> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            json!({
                "id": id.to_string(),
                "name": r.get::<String, _>("name"),
                "primary_ip": r.get::<String, _>("primary_ip"),
                "hostname": r.try_get::<Option<String>, _>("hostname").ok().flatten(),
                "os_family": r.get::<String, _>("os_family"),
                "os_distribution": r.try_get::<Option<String>, _>("os_distribution").ok().flatten(),
                "os_version": r.try_get::<Option<String>, _>("os_version").ok().flatten(),
                "cpu_cores": r.try_get::<Option<i32>, _>("cpu_cores").ok().flatten(),
                "total_ram_gb": r.try_get::<Option<i32>, _>("total_ram_gb").ok().flatten(),
                "total_disk_gb": r.try_get::<Option<i32>, _>("total_disk_gb").ok().flatten(),
                "has_gpu": r.get::<bool, _>("has_gpu"),
                "gpu_kind": r.try_get::<Option<String>, _>("gpu_kind").ok().flatten(),
                "gpu_count": r.try_get::<Option<i32>, _>("gpu_count").ok().flatten(),
                "gpu_model": r.try_get::<Option<String>, _>("gpu_model").ok().flatten(),
                "gpu_total_vram_gb": r.try_get::<Option<f64>, _>("gpu_total_vram_gb").ok().flatten(),
                "status": r.get::<String, _>("status"),
                "enrolled_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("enrolled_at").ok().flatten()),
                "last_seen_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_seen_at").ok().flatten()),
                "offline_since": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("offline_since").ok().flatten()),
                "status_changed_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("status_changed_at").ok().flatten()),
                "member_role": r.try_get::<Option<String>, _>("member_role").ok().flatten(),
                "member_runtime": r.try_get::<Option<String>, _>("member_runtime").ok().flatten(),
                "election_priority": r.try_get::<Option<i32>, _>("election_priority").ok().flatten(),
                "active_deployment_count": r.try_get::<Option<i64>, _>("active_deployment_count").ok().flatten().unwrap_or(0),
                "latest_cpu_pct": r.try_get::<Option<f64>, _>("latest_cpu_pct").ok().flatten(),
                "latest_ram_pct": r.try_get::<Option<f64>, _>("latest_ram_pct").ok().flatten(),
                "latest_disk_free_gb": r.try_get::<Option<f64>, _>("latest_disk_free_gb").ok().flatten(),
                "latest_recorded_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("latest_recorded_at").ok().flatten()),
            })
        })
        .collect();

    Ok(Json(json!({ "computers": computers })))
}

// ─── /api/fleet/members ─────────────────────────────────────────────────

pub async fn list_members(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT
            fm.computer_id,
            fm.role,
            fm.election_priority,
            fm.gh_account,
            fm.runtime,
            fm.models_dir,
            fm.disk_quota_pct,
            fm.enrolled_at,
            c.name,
            c.primary_ip,
            c.hostname,
            c.os_family,
            c.status,
            c.last_seen_at
        FROM fleet_members fm
        JOIN computers c ON c.id = fm.computer_id
        ORDER BY fm.election_priority DESC, c.name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("list_members", e))?;

    let members: Vec<Value> = rows
        .iter()
        .map(|r| {
            let computer_id: uuid::Uuid = r.get("computer_id");
            json!({
                "computer_id": computer_id.to_string(),
                "name": r.get::<String, _>("name"),
                "primary_ip": r.get::<String, _>("primary_ip"),
                "hostname": r.try_get::<Option<String>, _>("hostname").ok().flatten(),
                "os_family": r.get::<String, _>("os_family"),
                "status": r.get::<String, _>("status"),
                "role": r.get::<String, _>("role"),
                "election_priority": r.get::<i32, _>("election_priority"),
                "gh_account": r.try_get::<Option<String>, _>("gh_account").ok().flatten(),
                "runtime": r.get::<String, _>("runtime"),
                "models_dir": r.try_get::<Option<String>, _>("models_dir").ok().flatten(),
                "disk_quota_pct": r.get::<i32, _>("disk_quota_pct"),
                "enrolled_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("enrolled_at").ok().flatten()),
                "last_seen_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_seen_at").ok().flatten()),
            })
        })
        .collect();

    Ok(Json(json!({ "members": members })))
}

// ─── /api/fleet/leader ──────────────────────────────────────────────────

pub async fn get_leader(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;

    let leader_row = sqlx::query(
        r#"
        SELECT fls.computer_id, fls.member_name, fls.epoch, fls.elected_at,
               fls.reason, fls.heartbeat_at,
               c.primary_ip, c.status
        FROM fleet_leader_state fls
        LEFT JOIN computers c ON c.id = fls.computer_id
        WHERE fls.singleton_key = 'current'
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| db_err("get_leader", e))?;

    let leader = leader_row.map(|r| {
        let computer_id: uuid::Uuid = r.get("computer_id");
        let heartbeat_at: chrono::DateTime<chrono::Utc> = r.get("heartbeat_at");
        let age_seconds = (chrono::Utc::now() - heartbeat_at).num_seconds();
        json!({
            "computer_id": computer_id.to_string(),
            "member_name": r.get::<String, _>("member_name"),
            "epoch": r.get::<i64, _>("epoch"),
            "elected_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("elected_at"))),
            "reason": r.try_get::<Option<String>, _>("reason").ok().flatten(),
            "heartbeat_at": iso(Some(heartbeat_at)),
            "heartbeat_age_seconds": age_seconds,
            "primary_ip": r.try_get::<Option<String>, _>("primary_ip").ok().flatten(),
            "status": r.try_get::<Option<String>, _>("status").ok().flatten(),
        })
    });

    let candidate_rows = sqlx::query(
        r#"
        SELECT fm.computer_id, fm.role, fm.election_priority,
               c.name, c.primary_ip, c.status, c.last_seen_at
        FROM fleet_members fm
        JOIN computers c ON c.id = fm.computer_id
        ORDER BY fm.election_priority DESC, c.name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("leader_candidates", e))?;

    let candidates: Vec<Value> = candidate_rows
        .iter()
        .map(|r| {
            let computer_id: uuid::Uuid = r.get("computer_id");
            json!({
                "computer_id": computer_id.to_string(),
                "name": r.get::<String, _>("name"),
                "primary_ip": r.get::<String, _>("primary_ip"),
                "status": r.get::<String, _>("status"),
                "role": r.get::<String, _>("role"),
                "election_priority": r.get::<i32, _>("election_priority"),
                "last_seen_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_seen_at").ok().flatten()),
            })
        })
        .collect();

    Ok(Json(json!({
        "leader": leader,
        "candidates": candidates,
    })))
}

// ─── /api/fleet/health ──────────────────────────────────────────────────

pub async fn fleet_health(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT c.id, c.name, c.status, c.last_seen_at, c.offline_since,
               c.status_changed_at,
               (
                   SELECT COUNT(*) FROM computer_downtime_events d
                   WHERE d.computer_id = c.id AND d.online_at IS NULL
               ) AS open_downtime_events
        FROM computers c
        ORDER BY c.name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("fleet_health", e))?;

    let computers: Vec<Value> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            let last_seen: Option<chrono::DateTime<chrono::Utc>> = r
                .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_seen_at")
                .ok()
                .flatten();
            let pulse_age_seconds = last_seen.map(|t| (chrono::Utc::now() - t).num_seconds());
            let pulse_status = match pulse_age_seconds {
                Some(s) if s < 120 => "fresh",
                Some(s) if s < 600 => "stale",
                Some(_) => "expired",
                None => "unknown",
            };
            json!({
                "id": id.to_string(),
                "name": r.get::<String, _>("name"),
                "status": r.get::<String, _>("status"),
                "last_seen_at": iso(last_seen),
                "offline_since": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("offline_since").ok().flatten()),
                "status_changed_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("status_changed_at").ok().flatten()),
                "pulse_age_seconds": pulse_age_seconds,
                "pulse_status": pulse_status,
                "open_downtime_events": r.try_get::<Option<i64>, _>("open_downtime_events").ok().flatten().unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(json!({ "computers": computers })))
}

// ─── /api/llm/servers ───────────────────────────────────────────────────
//
// Backed by live Redis Pulse beats (via `PulseLlmRouter`), NOT Postgres —
// Postgres's `computer_model_deployments` table only reflects deployments
// that the materializer was able to upsert, which historically excluded
// pulse-discovered models not present in `model_catalog`. Reality for
// "what's running right now?" lives in Redis.
pub async fn llm_servers(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(router) = state.pulse_router.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "pulse router not available"})),
        ));
    };

    let raw = router.list_servers().await.map_err(|e| {
        tracing::error!("pulse api error (llm_servers): {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("llm_servers: {e}")})),
        )
    })?;

    // Shape the raw Pulse list into the dashboard-expected format:
    //   { computer, endpoint, runtime, model, queue_depth, tokens_per_sec,
    //     healthy, status }
    let servers: Vec<Value> = raw
        .into_iter()
        .map(|v| {
            json!({
                "computer": v.get("computer").cloned().unwrap_or(Value::Null),
                "endpoint": v.get("endpoint").cloned().unwrap_or(Value::Null),
                "runtime": v.get("runtime").cloned().unwrap_or(Value::Null),
                "model": v.get("model").cloned().unwrap_or(Value::Null),
                "queue_depth": v.get("queue_depth").cloned().unwrap_or(Value::Null),
                "tokens_per_sec": v.get("tokens_per_sec_last_min").cloned().unwrap_or(Value::Null),
                "healthy": v.get("healthy").cloned().unwrap_or(Value::Null),
                "status": v.get("status").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    Ok(Json(json!({ "servers": servers })))
}

// ─── /api/software/computers ────────────────────────────────────────────

pub async fn software_computers(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT cs.computer_id, cs.software_id, cs.installed_version,
               cs.install_source, cs.install_source_identifier,
               cs.install_path, cs.first_seen_at, cs.last_checked_at,
               cs.last_upgraded_at, cs.status, cs.last_upgrade_error,
               cs.consecutive_failures,
               sr.display_name AS software_display_name,
               sr.kind AS software_kind,
               sr.latest_version AS latest_version,
               sr.requires_restart, sr.requires_reboot,
               c.name AS computer_name, c.os_family
        FROM computer_software cs
        JOIN software_registry sr ON sr.id = cs.software_id
        JOIN computers c ON c.id = cs.computer_id
        ORDER BY c.name, sr.display_name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("software_computers", e))?;

    let rows_json: Vec<Value> = rows
        .iter()
        .map(|r| {
            let computer_id: uuid::Uuid = r.get("computer_id");
            let installed = r.try_get::<Option<String>, _>("installed_version").ok().flatten();
            let latest = r.try_get::<Option<String>, _>("latest_version").ok().flatten();
            let status = r.get::<String, _>("status");
            let drift = match (&installed, &latest) {
                (Some(i), Some(l)) => i != l,
                _ => false,
            } || status == "upgrade_available";
            json!({
                "computer_id": computer_id.to_string(),
                "computer_name": r.get::<String, _>("computer_name"),
                "os_family": r.get::<String, _>("os_family"),
                "software_id": r.get::<String, _>("software_id"),
                "software_display_name": r.get::<String, _>("software_display_name"),
                "software_kind": r.get::<String, _>("software_kind"),
                "installed_version": installed,
                "latest_version": latest,
                "install_source": r.try_get::<Option<String>, _>("install_source").ok().flatten(),
                "install_source_identifier": r.try_get::<Option<String>, _>("install_source_identifier").ok().flatten(),
                "install_path": r.try_get::<Option<String>, _>("install_path").ok().flatten(),
                "first_seen_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("first_seen_at").ok().flatten()),
                "last_checked_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_checked_at").ok().flatten()),
                "last_upgraded_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_upgraded_at").ok().flatten()),
                "status": status,
                "last_upgrade_error": r.try_get::<Option<String>, _>("last_upgrade_error").ok().flatten(),
                "consecutive_failures": r.try_get::<i32, _>("consecutive_failures").unwrap_or(0),
                "requires_restart": r.get::<bool, _>("requires_restart"),
                "requires_reboot": r.get::<bool, _>("requires_reboot"),
                "drift": drift,
            })
        })
        .collect();

    Ok(Json(json!({ "rows": rows_json })))
}

// ─── /api/software/drift ────────────────────────────────────────────────

pub async fn software_drift(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT cs.computer_id, cs.software_id, cs.installed_version,
               cs.install_source, cs.status, cs.last_checked_at,
               sr.display_name AS software_display_name,
               sr.latest_version, sr.requires_restart, sr.requires_reboot,
               c.name AS computer_name, c.os_family
        FROM computer_software cs
        JOIN software_registry sr ON sr.id = cs.software_id
        JOIN computers c ON c.id = cs.computer_id
        WHERE cs.status = 'upgrade_available'
           OR (sr.latest_version IS NOT NULL
               AND cs.installed_version IS NOT NULL
               AND cs.installed_version <> sr.latest_version)
        ORDER BY c.name, sr.display_name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("software_drift", e))?;

    let rows_json: Vec<Value> = rows
        .iter()
        .map(|r| {
            let computer_id: uuid::Uuid = r.get("computer_id");
            json!({
                "computer_id": computer_id.to_string(),
                "computer_name": r.get::<String, _>("computer_name"),
                "os_family": r.get::<String, _>("os_family"),
                "software_id": r.get::<String, _>("software_id"),
                "software_display_name": r.get::<String, _>("software_display_name"),
                "installed_version": r.try_get::<Option<String>, _>("installed_version").ok().flatten(),
                "latest_version": r.try_get::<Option<String>, _>("latest_version").ok().flatten(),
                "install_source": r.try_get::<Option<String>, _>("install_source").ok().flatten(),
                "status": r.get::<String, _>("status"),
                "last_checked_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_checked_at").ok().flatten()),
                "requires_restart": r.get::<bool, _>("requires_restart"),
                "requires_reboot": r.get::<bool, _>("requires_reboot"),
            })
        })
        .collect();

    Ok(Json(json!({ "rows": rows_json })))
}

// ─── /api/projects ──────────────────────────────────────────────────────

pub async fn list_projects(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT p.id, p.display_name, p.compose_file, p.repo_url,
               p.default_branch, p.main_commit_sha, p.main_commit_message,
               p.main_committed_at, p.main_committed_by, p.main_last_synced_at,
               p.target_computers, p.health_endpoint, p.status,
               (
                   SELECT COUNT(*) FROM project_branches pb
                   WHERE pb.project_id = p.id AND pb.status = 'active'
               ) AS active_branch_count,
               (
                   SELECT COALESCE(
                       jsonb_agg(
                           jsonb_build_object(
                               'id', pe.id,
                               'name', pe.name,
                               'deployed_commit_sha', pe.deployed_commit_sha,
                               'deployed_at', pe.deployed_at,
                               'deploy_status', pe.deploy_status,
                               'health_status', pe.health_status,
                               'url', pe.url
                           ) ORDER BY pe.name
                       ),
                       '[]'::jsonb
                   )
                   FROM project_environments pe
                   WHERE pe.project_id = p.id
               ) AS environments
        FROM projects p
        ORDER BY p.display_name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("list_projects", e))?;

    let projects: Vec<Value> = rows
        .iter()
        .map(|r| {
            let envs: serde_json::Value = r
                .try_get::<serde_json::Value, _>("environments")
                .unwrap_or(json!([]));
            let targets: serde_json::Value = r
                .try_get::<serde_json::Value, _>("target_computers")
                .unwrap_or(json!([]));
            json!({
                "id": r.get::<String, _>("id"),
                "display_name": r.get::<String, _>("display_name"),
                "compose_file": r.try_get::<Option<String>, _>("compose_file").ok().flatten(),
                "repo_url": r.try_get::<Option<String>, _>("repo_url").ok().flatten(),
                "default_branch": r.get::<String, _>("default_branch"),
                "main_commit_sha": r.try_get::<Option<String>, _>("main_commit_sha").ok().flatten(),
                "main_commit_message": r.try_get::<Option<String>, _>("main_commit_message").ok().flatten(),
                "main_committed_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("main_committed_at").ok().flatten()),
                "main_committed_by": r.try_get::<Option<String>, _>("main_committed_by").ok().flatten(),
                "main_last_synced_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("main_last_synced_at").ok().flatten()),
                "target_computers": targets,
                "health_endpoint": r.try_get::<Option<String>, _>("health_endpoint").ok().flatten(),
                "status": r.get::<String, _>("status"),
                "active_branch_count": r.try_get::<Option<i64>, _>("active_branch_count").ok().flatten().unwrap_or(0),
                "environments": envs,
            })
        })
        .collect();

    Ok(Json(json!({ "projects": projects })))
}

// ─── /api/projects/{id}/branches ────────────────────────────────────────

pub async fn project_branches(
    Path(project_id): Path<String>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT id, project_id, branch_name, created_by, assigned_computer,
               assigned_agent, purpose, last_commit_sha, last_commit_message,
               last_commit_at, pr_number, pr_url, pr_state, status,
               merged_at, merged_sha
        FROM project_branches
        WHERE project_id = $1
        ORDER BY
            CASE status WHEN 'active' THEN 0 ELSE 1 END,
            last_commit_at DESC NULLS LAST,
            branch_name
        "#,
    )
    .bind(&project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("project_branches", e))?;

    let branches: Vec<Value> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            json!({
                "id": id.to_string(),
                "project_id": r.get::<String, _>("project_id"),
                "branch_name": r.get::<String, _>("branch_name"),
                "created_by": r.get::<String, _>("created_by"),
                "assigned_computer": r.try_get::<Option<String>, _>("assigned_computer").ok().flatten(),
                "assigned_agent": r.try_get::<Option<String>, _>("assigned_agent").ok().flatten(),
                "purpose": r.try_get::<Option<String>, _>("purpose").ok().flatten(),
                "last_commit_sha": r.try_get::<Option<String>, _>("last_commit_sha").ok().flatten(),
                "last_commit_message": r.try_get::<Option<String>, _>("last_commit_message").ok().flatten(),
                "last_commit_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_commit_at").ok().flatten()),
                "pr_number": r.try_get::<Option<i32>, _>("pr_number").ok().flatten(),
                "pr_url": r.try_get::<Option<String>, _>("pr_url").ok().flatten(),
                "pr_state": r.try_get::<Option<String>, _>("pr_state").ok().flatten(),
                "status": r.get::<String, _>("status"),
                "merged_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("merged_at").ok().flatten()),
                "merged_sha": r.try_get::<Option<String>, _>("merged_sha").ok().flatten(),
            })
        })
        .collect();

    Ok(Json(json!({ "branches": branches })))
}

// ─── /api/pm/work-items ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WorkItemQuery {
    pub project: Option<String>,
    pub status: Option<String>,
    pub assigned_to: Option<String>,
    pub limit: Option<i64>,
}

pub async fn list_work_items(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<WorkItemQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);

    let mut sql = String::from(
        r#"
        SELECT id, project_id, milestone_id, parent_id, kind, title, description,
               labels, status, priority, assigned_to, assigned_computer,
               branch_name, pr_url, created_at, created_by, started_at,
               completed_at, due_date, estimated_hours
        FROM work_items
        WHERE 1=1
        "#,
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(p) = q.project.as_ref() {
        args.push(p.clone());
        sql.push_str(&format!(" AND project_id = ${}", args.len()));
    }
    if let Some(s) = q.status.as_ref() {
        args.push(s.clone());
        sql.push_str(&format!(" AND status = ${}", args.len()));
    }
    if let Some(a) = q.assigned_to.as_ref() {
        args.push(a.clone());
        sql.push_str(&format!(" AND assigned_to = ${}", args.len()));
    }
    sql.push_str(&format!(" ORDER BY created_at DESC LIMIT {}", limit));

    let mut query = sqlx::query(&sql);
    for a in &args {
        query = query.bind(a);
    }
    let rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| db_err("list_work_items", e))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            let labels: serde_json::Value = r
                .try_get::<serde_json::Value, _>("labels")
                .unwrap_or(json!([]));
            json!({
                "id": id.to_string(),
                "project_id": r.get::<String, _>("project_id"),
                "milestone_id": r.try_get::<Option<uuid::Uuid>, _>("milestone_id").ok().flatten().map(|u| u.to_string()),
                "parent_id": r.try_get::<Option<uuid::Uuid>, _>("parent_id").ok().flatten().map(|u| u.to_string()),
                "kind": r.get::<String, _>("kind"),
                "title": r.get::<String, _>("title"),
                "description": r.try_get::<Option<String>, _>("description").ok().flatten(),
                "labels": labels,
                "status": r.get::<String, _>("status"),
                "priority": r.get::<String, _>("priority"),
                "assigned_to": r.try_get::<Option<String>, _>("assigned_to").ok().flatten(),
                "assigned_computer": r.try_get::<Option<String>, _>("assigned_computer").ok().flatten(),
                "branch_name": r.try_get::<Option<String>, _>("branch_name").ok().flatten(),
                "pr_url": r.try_get::<Option<String>, _>("pr_url").ok().flatten(),
                "created_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"))),
                "created_by": r.get::<String, _>("created_by"),
                "started_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at").ok().flatten()),
                "completed_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at").ok().flatten()),
                "estimated_hours": r.try_get::<Option<f64>, _>("estimated_hours").ok().flatten(),
            })
        })
        .collect();

    Ok(Json(json!({ "work_items": items })))
}

// ─── /api/alerts/policies ───────────────────────────────────────────────

pub async fn alert_policies(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT id, name, description, metric, scope, scope_computer_id,
               condition, duration_secs, severity, cooldown_secs, channel,
               enabled, created_at
        FROM alert_policies
        ORDER BY severity, name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("alert_policies", e))?;

    let policies: Vec<Value> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            json!({
                "id": id.to_string(),
                "name": r.get::<String, _>("name"),
                "description": r.try_get::<Option<String>, _>("description").ok().flatten(),
                "metric": r.get::<String, _>("metric"),
                "scope": r.get::<String, _>("scope"),
                "scope_computer_id": r.try_get::<Option<uuid::Uuid>, _>("scope_computer_id").ok().flatten().map(|u| u.to_string()),
                "condition": r.get::<String, _>("condition"),
                "duration_secs": r.get::<i32, _>("duration_secs"),
                "severity": r.get::<String, _>("severity"),
                "cooldown_secs": r.get::<i32, _>("cooldown_secs"),
                "channel": r.get::<String, _>("channel"),
                "enabled": r.get::<bool, _>("enabled"),
                "created_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"))),
            })
        })
        .collect();

    Ok(Json(json!({ "policies": policies })))
}

// ─── /api/alerts/events ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AlertEventsQuery {
    pub active: Option<bool>,
    pub limit: Option<i64>,
}

pub async fn alert_events(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<AlertEventsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let only_active = q.active.unwrap_or(false);

    let sql = format!(
        r#"
        SELECT ae.id, ae.policy_id, ae.computer_id, ae.fired_at, ae.resolved_at,
               ae.value, ae.value_text, ae.message, ae.channel_result,
               ap.name AS policy_name, ap.severity, ap.metric,
               c.name AS computer_name
        FROM alert_events ae
        JOIN alert_policies ap ON ap.id = ae.policy_id
        LEFT JOIN computers c ON c.id = ae.computer_id
        {}
        ORDER BY ae.fired_at DESC
        LIMIT {}
        "#,
        if only_active {
            "WHERE ae.resolved_at IS NULL"
        } else {
            ""
        },
        limit,
    );

    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| db_err("alert_events", e))?;

    let events: Vec<Value> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            let policy_id: uuid::Uuid = r.get("policy_id");
            json!({
                "id": id.to_string(),
                "policy_id": policy_id.to_string(),
                "policy_name": r.get::<String, _>("policy_name"),
                "severity": r.get::<String, _>("severity"),
                "metric": r.get::<String, _>("metric"),
                "computer_id": r.try_get::<Option<uuid::Uuid>, _>("computer_id").ok().flatten().map(|u| u.to_string()),
                "computer_name": r.try_get::<Option<String>, _>("computer_name").ok().flatten(),
                "fired_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("fired_at"))),
                "resolved_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("resolved_at").ok().flatten()),
                "value": r.try_get::<Option<f64>, _>("value").ok().flatten(),
                "value_text": r.try_get::<Option<String>, _>("value_text").ok().flatten(),
                "message": r.try_get::<Option<String>, _>("message").ok().flatten(),
                "channel_result": r.try_get::<Option<String>, _>("channel_result").ok().flatten(),
            })
        })
        .collect();

    Ok(Json(json!({ "events": events })))
}

// ─── /api/metrics/{computer}/history ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    pub hours: Option<i64>,
}

pub async fn metrics_history(
    Path(computer): Path<String>,
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<MetricsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let hours = q.hours.unwrap_or(24).clamp(1, 720);

    // Accept either the computer name or UUID.
    let computer_id: uuid::Uuid = if let Ok(id) = uuid::Uuid::parse_str(&computer) {
        id
    } else {
        let r = sqlx::query("SELECT id FROM computers WHERE name = $1")
            .bind(&computer)
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err("metrics_history_lookup", e))?;
        match r {
            Some(row) => row.get("id"),
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("computer '{computer}' not found")})),
                ));
            }
        }
    };

    let rows = sqlx::query(
        r#"
        SELECT recorded_at, cpu_pct, ram_pct, ram_used_gb, disk_free_gb,
               gpu_pct, llm_ram_allocated_gb, llm_queue_depth,
               llm_active_requests, llm_tokens_per_sec
        FROM computer_metrics_history
        WHERE computer_id = $1
          AND recorded_at > NOW() - ($2::text || ' hours')::interval
        ORDER BY recorded_at ASC
        "#,
    )
    .bind(computer_id)
    .bind(hours.to_string())
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("metrics_history", e))?;

    let points: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "recorded_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("recorded_at"))),
                "cpu_pct": r.try_get::<Option<f64>, _>("cpu_pct").ok().flatten(),
                "ram_pct": r.try_get::<Option<f64>, _>("ram_pct").ok().flatten(),
                "ram_used_gb": r.try_get::<Option<f64>, _>("ram_used_gb").ok().flatten(),
                "disk_free_gb": r.try_get::<Option<f64>, _>("disk_free_gb").ok().flatten(),
                "gpu_pct": r.try_get::<Option<f64>, _>("gpu_pct").ok().flatten(),
                "llm_ram_allocated_gb": r.try_get::<Option<f64>, _>("llm_ram_allocated_gb").ok().flatten(),
                "llm_queue_depth": r.try_get::<Option<i32>, _>("llm_queue_depth").ok().flatten(),
                "llm_active_requests": r.try_get::<Option<i32>, _>("llm_active_requests").ok().flatten(),
                "llm_tokens_per_sec": r.try_get::<Option<f64>, _>("llm_tokens_per_sec").ok().flatten(),
            })
        })
        .collect();

    Ok(Json(json!({
        "computer_id": computer_id.to_string(),
        "hours": hours,
        "points": points,
    })))
}

// ─── /api/ha/status ─────────────────────────────────────────────────────

pub async fn ha_status(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;

    let replicas = sqlx::query(
        r#"
        SELECT dr.computer_id, dr.database_kind, dr.role, dr.status,
               dr.lag_bytes, dr.last_sync_at, dr.promoted_at,
               dr.bootstrapped_from_backup_id, dr.notes,
               c.name AS computer_name, c.primary_ip
        FROM database_replicas dr
        JOIN computers c ON c.id = dr.computer_id
        ORDER BY dr.database_kind,
            CASE dr.role
                WHEN 'primary' THEN 0
                WHEN 'replica' THEN 1
                ELSE 2
            END,
            c.name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("ha_replicas", e))?;

    let replica_rows: Vec<Value> = replicas
        .iter()
        .map(|r| {
            let computer_id: uuid::Uuid = r.get("computer_id");
            let bootstrapped: Option<uuid::Uuid> = r
                .try_get::<Option<uuid::Uuid>, _>("bootstrapped_from_backup_id")
                .ok()
                .flatten();
            json!({
                "computer_id": computer_id.to_string(),
                "computer_name": r.get::<String, _>("computer_name"),
                "primary_ip": r.get::<String, _>("primary_ip"),
                "database_kind": r.get::<String, _>("database_kind"),
                "role": r.get::<String, _>("role"),
                "status": r.get::<String, _>("status"),
                "lag_bytes": r.try_get::<Option<i64>, _>("lag_bytes").ok().flatten(),
                "last_sync_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_sync_at").ok().flatten()),
                "promoted_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("promoted_at").ok().flatten()),
                "bootstrapped_from_backup_id": bootstrapped.map(|u| u.to_string()),
                "notes": r.try_get::<Option<String>, _>("notes").ok().flatten(),
            })
        })
        .collect();

    let backups = sqlx::query(
        r#"
        SELECT b.id, b.database_kind, b.created_at, b.size_bytes,
               b.source_computer_id, b.checksum_sha256, b.file_name,
               b.distribution_status, b.verified_restorable_at,
               b.retention_tier,
               c.name AS source_computer_name
        FROM backups b
        LEFT JOIN computers c ON c.id = b.source_computer_id
        ORDER BY b.created_at DESC
        LIMIT 20
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("ha_backups", e))?;

    let backup_rows: Vec<Value> = backups
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            let source_id: uuid::Uuid = r.get("source_computer_id");
            let created_at: chrono::DateTime<chrono::Utc> = r.get("created_at");
            let age_seconds = (chrono::Utc::now() - created_at).num_seconds();
            let dist: serde_json::Value = r
                .try_get::<serde_json::Value, _>("distribution_status")
                .unwrap_or(json!({}));
            json!({
                "id": id.to_string(),
                "database_kind": r.get::<String, _>("database_kind"),
                "created_at": iso(Some(created_at)),
                "age_seconds": age_seconds,
                "size_bytes": r.get::<i64, _>("size_bytes"),
                "source_computer_id": source_id.to_string(),
                "source_computer_name": r.try_get::<Option<String>, _>("source_computer_name").ok().flatten(),
                "checksum_sha256": r.get::<String, _>("checksum_sha256"),
                "file_name": r.get::<String, _>("file_name"),
                "distribution_status": dist,
                "verified_restorable_at": iso(r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("verified_restorable_at").ok().flatten()),
                "retention_tier": r.get::<String, _>("retention_tier"),
            })
        })
        .collect();

    Ok(Json(json!({
        "replicas": replica_rows,
        "backups": backup_rows,
    })))
}

// ─── /api/docker/projects ───────────────────────────────────────────────

pub async fn docker_projects(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT dc.id, dc.computer_id, dc.project_name, dc.compose_file,
               dc.container_name, dc.container_id, dc.image, dc.ports,
               dc.status, dc.health, dc.last_status_change,
               dc.first_seen_at, dc.last_seen_at,
               c.name AS computer_name, c.primary_ip
        FROM computer_docker_containers dc
        JOIN computers c ON c.id = dc.computer_id
        ORDER BY COALESCE(dc.project_name, '~no-project'), c.name, dc.container_name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_err("docker_projects", e))?;

    // Group containers by compose project.
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    for r in &rows {
        let project = r
            .try_get::<Option<String>, _>("project_name")
            .ok()
            .flatten()
            .unwrap_or_else(|| "(none)".to_string());
        let id: uuid::Uuid = r.get("id");
        let computer_id: uuid::Uuid = r.get("computer_id");
        let ports: serde_json::Value = r
            .try_get::<serde_json::Value, _>("ports")
            .unwrap_or(json!([]));
        let container = json!({
            "id": id.to_string(),
            "computer_id": computer_id.to_string(),
            "computer_name": r.get::<String, _>("computer_name"),
            "primary_ip": r.get::<String, _>("primary_ip"),
            "project_name": r.try_get::<Option<String>, _>("project_name").ok().flatten(),
            "compose_file": r.try_get::<Option<String>, _>("compose_file").ok().flatten(),
            "container_name": r.get::<String, _>("container_name"),
            "container_id": r.try_get::<Option<String>, _>("container_id").ok().flatten(),
            "image": r.try_get::<Option<String>, _>("image").ok().flatten(),
            "ports": ports,
            "status": r.get::<String, _>("status"),
            "health": r.try_get::<Option<String>, _>("health").ok().flatten(),
            "last_status_change": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("last_status_change"))),
            "first_seen_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("first_seen_at"))),
            "last_seen_at": iso(Some(r.get::<chrono::DateTime<chrono::Utc>, _>("last_seen_at"))),
        });
        groups.entry(project).or_default().push(container);
    }

    let projects: Vec<Value> = groups
        .into_iter()
        .map(|(project, containers)| {
            let running_count = containers
                .iter()
                .filter(|c| c.get("status").and_then(|v| v.as_str()) == Some("running"))
                .count();
            json!({
                "project_name": project,
                "container_count": containers.len(),
                "running_count": running_count,
                "containers": containers,
            })
        })
        .collect();

    Ok(Json(json!({ "projects": projects })))
}

// ─── /api/events/stream ────────────────────────────────────────────────
//
// SSE endpoint bridging NATS `fleet.events.>` into HTTP. Each inbound
// NATS message is fanned out as a single `data: { ... }` SSE line with
// a JSON envelope `{ subject, payload, received_at }`.
//
// Returns 503 with a JSON error if NATS isn't reachable. The frontend
// (`useFleetEvents`) treats 503 as a signal to keep polling.

pub async fn events_stream(
    State(_state): State<Arc<GatewayState>>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    // Open a NATS connection lazily via ff-pulse's shared client. Same
    // URL resolution as the rest of the fleet (`FORGEFLEET_NATS_URL`).
    let Some(client) = ff_pulse::nats::get_or_init_nats().await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "nats unavailable",
                "hint": "FORGEFLEET_NATS_URL is unreachable; frontend will fall back to polling",
            })),
        ));
    };

    // Subscribe to every fleet event subject. If the subscribe itself
    // fails (e.g. server was up at connect-time but has since gone
    // away), surface 503 too.
    let mut sub = match client.subscribe("fleet.events.>").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("pulse events_stream: NATS subscribe failed: {e}");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": format!("nats subscribe failed: {e}")})),
            ));
        }
    };

    // Bridge NATS messages into an mpsc channel we can wrap as a stream.
    // Bounded channel acts as back-pressure — if the client is slow we
    // drop events rather than buffering unboundedly in server memory.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        use futures::StreamExt;
        while let Some(msg) = sub.next().await {
            let subject = msg.subject.to_string();
            // Try to parse payload as JSON; fall back to raw string.
            let payload: Value = serde_json::from_slice(&msg.payload)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&msg.payload).into()));
            let envelope = json!({
                "subject": subject,
                "payload": payload,
                "received_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            });
            let event = Event::default().data(envelope.to_string());
            if tx.send(Ok(event)).await.is_err() {
                // Client disconnected.
                break;
            }
        }
    });

    let stream: ReceiverStream<Result<Event, Infallible>> = ReceiverStream::new(rx);
    // Cast to the `Stream` trait so axum can consume it.
    let stream: Box<dyn Stream<Item = Result<Event, Infallible>> + Send + Unpin> = Box::new(stream);

    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response())
}
