//! Projects-first PM queries (Postgres).
//!
//! Stage 1 of the PM consolidation (see `.forgefleet/plans/pm-consolidation.md`):
//! a project attaches MANY GitHub locations ([`ProjectRepo`]) and MANY local
//! folders ([`ProjectFolder`], per-host or canonical). Backed by the V141
//! `project_repos` / `project_folders` tables. This is the read/write layer the
//! gateway's `/api/pm/projects*` routes and (later) the `ff pm` CLI share, so
//! both surfaces speak the one Postgres model instead of the legacy single
//! `projects.repo_url` + SQLite mission-control split.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::error::Result;

/// A GitHub location attached to a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRepo {
    pub id: String,
    pub project_id: String,
    pub github_url: String,
    pub name: Option<String>,
    pub default_branch: String,
    pub role: Option<String>,
    pub is_primary: bool,
    pub created_at: String,
}

/// A local folder attached to a project. `computer_id` / `computer_name` are
/// `None` for a canonical path that applies to every host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFolder {
    pub id: String,
    pub project_id: String,
    pub computer_id: Option<String>,
    pub computer_name: Option<String>,
    pub path: String,
    pub role: Option<String>,
    pub is_primary: bool,
    pub created_at: String,
}

/// A project plus its attached repos and folders — the row the Projects-first
/// web index renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWithAttachments {
    pub id: String,
    pub display_name: String,
    pub status: String,
    pub repos: Vec<ProjectRepo>,
    pub folders: Vec<ProjectFolder>,
}

fn fmt_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn repo_from_row(r: &sqlx::postgres::PgRow) -> ProjectRepo {
    ProjectRepo {
        id: r.get::<uuid::Uuid, _>("id").to_string(),
        project_id: r.get("project_id"),
        github_url: r.get("github_url"),
        name: r.try_get("name").ok(),
        default_branch: r.get("default_branch"),
        role: r.try_get("role").ok(),
        is_primary: r.get("is_primary"),
        created_at: fmt_ts(r.get("created_at")),
    }
}

fn folder_from_row(r: &sqlx::postgres::PgRow) -> ProjectFolder {
    ProjectFolder {
        id: r.get::<uuid::Uuid, _>("id").to_string(),
        project_id: r.get("project_id"),
        computer_id: r
            .try_get::<Option<uuid::Uuid>, _>("computer_id")
            .ok()
            .flatten()
            .map(|u| u.to_string()),
        computer_name: r.try_get("computer_name").ok(),
        path: r.get("path"),
        role: r.try_get("role").ok(),
        is_primary: r.get("is_primary"),
        created_at: fmt_ts(r.get("created_at")),
    }
}

// ─── repos ──────────────────────────────────────────────────────────────────

/// All GitHub locations for a project, primary first.
pub async fn pg_list_project_repos(pool: &PgPool, project_id: &str) -> Result<Vec<ProjectRepo>> {
    let rows = sqlx::query(
        "SELECT id, project_id, github_url, name, default_branch, role, is_primary, created_at \
           FROM project_repos WHERE project_id = $1 \
          ORDER BY is_primary DESC, created_at ASC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(repo_from_row).collect())
}

/// Attach a GitHub location to a project. When `is_primary`, any existing
/// primary for the project is demoted first (the partial unique index allows at
/// most one). Idempotent on `(project_id, github_url)` — re-adding updates the
/// existing row's metadata.
pub async fn pg_add_project_repo(
    pool: &PgPool,
    project_id: &str,
    github_url: &str,
    name: Option<&str>,
    default_branch: &str,
    role: Option<&str>,
    is_primary: bool,
) -> Result<ProjectRepo> {
    let mut tx = pool.begin().await?;
    if is_primary {
        sqlx::query("UPDATE project_repos SET is_primary = FALSE WHERE project_id = $1")
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
    }
    let row = sqlx::query(
        "INSERT INTO project_repos (project_id, github_url, name, default_branch, role, is_primary) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (project_id, github_url) DO UPDATE SET \
             name = EXCLUDED.name, default_branch = EXCLUDED.default_branch, \
             role = EXCLUDED.role, is_primary = EXCLUDED.is_primary \
         RETURNING id, project_id, github_url, name, default_branch, role, is_primary, created_at",
    )
    .bind(project_id)
    .bind(github_url)
    .bind(name)
    .bind(default_branch)
    .bind(role)
    .bind(is_primary)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(repo_from_row(&row))
}

/// Detach a GitHub location by id. Returns whether a row was removed.
pub async fn pg_delete_project_repo(pool: &PgPool, id: &str) -> Result<bool> {
    let uid = uuid::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad repo id {id}: {e}")))?;
    let res = sqlx::query("DELETE FROM project_repos WHERE id = $1")
        .bind(uid)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ─── folders ─────────────────────────────────────────────────────────────────

/// All local folders for a project, primary first, with the host name resolved
/// for display (NULL host = canonical/all-hosts).
pub async fn pg_list_project_folders(
    pool: &PgPool,
    project_id: &str,
) -> Result<Vec<ProjectFolder>> {
    let rows = sqlx::query(
        "SELECT f.id, f.project_id, f.computer_id, c.name AS computer_name, \
                f.path, f.role, f.is_primary, f.created_at \
           FROM project_folders f \
           LEFT JOIN computers c ON c.id = f.computer_id \
          WHERE f.project_id = $1 \
          ORDER BY f.is_primary DESC, f.created_at ASC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(folder_from_row).collect())
}

/// Attach a local folder to a project. `computer_id` `None` = a canonical path
/// for every host; `Some` = that host's checkout. When `is_primary`, demotes any
/// existing primary first.
pub async fn pg_add_project_folder(
    pool: &PgPool,
    project_id: &str,
    computer_id: Option<&str>,
    path: &str,
    role: Option<&str>,
    is_primary: bool,
) -> Result<ProjectFolder> {
    let computer_uuid =
        match computer_id {
            Some(c) => Some(uuid::Uuid::parse_str(c).map_err(|e| {
                crate::error::DbError::NotFound(format!("bad computer id {c}: {e}"))
            })?),
            None => None,
        };
    let mut tx = pool.begin().await?;
    if is_primary {
        sqlx::query("UPDATE project_folders SET is_primary = FALSE WHERE project_id = $1")
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
    }
    let row = sqlx::query(
        "INSERT INTO project_folders (project_id, computer_id, path, role, is_primary) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, project_id, computer_id, \
                   (SELECT name FROM computers WHERE id = $2) AS computer_name, \
                   path, role, is_primary, created_at",
    )
    .bind(project_id)
    .bind(computer_uuid)
    .bind(path)
    .bind(role)
    .bind(is_primary)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(folder_from_row(&row))
}

/// Detach a local folder by id. Returns whether a row was removed.
pub async fn pg_delete_project_folder(pool: &PgPool, id: &str) -> Result<bool> {
    let uid = uuid::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad folder id {id}: {e}")))?;
    let res = sqlx::query("DELETE FROM project_folders WHERE id = $1")
        .bind(uid)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ─── projects index ──────────────────────────────────────────────────────────

/// Every project with its attached repos + folders — the Projects-first index.
/// One query per relation (3 total), assembled in Rust; the project list is
/// small (tens of rows) so this is cheaper than a wide join with array_agg.
pub async fn pg_list_projects_with_attachments(
    pool: &PgPool,
) -> Result<Vec<ProjectWithAttachments>> {
    let project_rows =
        sqlx::query("SELECT id, display_name, status FROM projects ORDER BY display_name ASC")
            .fetch_all(pool)
            .await?;

    let mut out = Vec::with_capacity(project_rows.len());
    for p in &project_rows {
        let id: String = p.get("id");
        let repos = pg_list_project_repos(pool, &id).await?;
        let folders = pg_list_project_folders(pool, &id).await?;
        out.push(ProjectWithAttachments {
            display_name: p.get("display_name"),
            status: p.get("status"),
            id,
            repos,
            folders,
        });
    }
    Ok(out)
}
