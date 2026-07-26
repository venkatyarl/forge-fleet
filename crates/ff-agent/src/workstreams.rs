//! Per-project session-of-record ("workstream") — the foundation for
//! ff-owns-the-session (2026-07-25).
//!
//! ff owns ONE durable workstream per project. External clients (this Claude
//! session on forge-fleet, or a Codex/Kimi session on HireFlow360, plus the TUI
//! and web) resolve their project — git-remote-first, walking UP to the project
//! root via [`crate::project_scope::resolve_from_dir`] — and ATTACH to that
//! project's workstream as a view/orchestrator, while ff does the actual build
//! work in the backend (work_items → local-first dispatch → self-heal → merge).
//!
//! This module keeps the `ff_workstreams` rows in existence (one per active
//! project). The attach + working-summary/reporting layers build on top: a
//! client reads its workstream's `working_summary` to report "what's happening"
//! and every project's work is tied together under its single workstream.

use anyhow::Result;
use sqlx::PgPool;

/// Idempotent: unique key on project_key so exactly one workstream exists per
/// project (the "single session for a project" invariant the operator wants).
pub async fn ensure_schema(pg: &PgPool) -> Result<()> {
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS ff_workstreams_project_key_uidx \
         ON ff_workstreams (project_key)",
    )
    .execute(pg)
    .await?;
    Ok(())
}

/// Ensure ONE workstream row per active project. Idempotent — fills identity
/// fields (project_key, git_remote from repo_url, basename) but NEVER clobbers
/// `goal`/`working_summary` (agent/operator-curated live state). Returns the
/// number of rows inserted-or-refreshed. Leader-gated by the caller.
pub async fn ensure_all_workstreams(pg: &PgPool) -> Result<u64> {
    ensure_schema(pg).await?;
    let n = sqlx::query(
        "INSERT INTO ff_workstreams \
            (id, project_key, git_remote, basename, aliases, goal, status, leader_generation, updated_at) \
         SELECT gen_random_uuid(), p.id, coalesce(p.repo_url, ''), \
                coalesce(nullif(p.display_name, ''), p.id), '[]'::jsonb, \
                coalesce(nullif(p.display_name, ''), p.id) || ' — project session-of-record', \
                'active', 0, now() \
           FROM projects p \
          WHERE p.status = 'active' \
         ON CONFLICT (project_key) DO UPDATE SET \
                git_remote = EXCLUDED.git_remote, \
                basename   = EXCLUDED.basename, \
                updated_at = now()",
    )
    .execute(pg)
    .await?
    .rows_affected();
    Ok(n)
}

/// A resolved workstream (the session-of-record a client attaches to).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Workstream {
    pub id: uuid::Uuid,
    pub project_key: String,
    pub git_remote: String,
    pub basename: String,
    pub working_summary: Option<String>,
    pub status: String,
}

/// Resolve the workstream for a project key (the session-of-record clients
/// attach to). `None` if the project has no workstream yet.
pub async fn workstream_for_project(pg: &PgPool, project_key: &str) -> Result<Option<Workstream>> {
    let ws = sqlx::query_as::<_, Workstream>(
        "SELECT id, project_key, git_remote, basename, working_summary, status \
           FROM ff_workstreams WHERE project_key = $1",
    )
    .bind(project_key)
    .fetch_optional(pg)
    .await?;
    Ok(ws)
}

// ---------------------------------------------------------------------------
// Attach + reporting (2026-07-26): make "ff owns the session" real end-to-end.
// A CLI (this Claude session on forge-fleet, or Codex/Kimi elsewhere) resolves
// its project from cwd, ATTACHES to that project's single workstream, and then
// continuously REPORTS its working state into it. The workstream becomes the
// shared source-of-record across all three CLIs + the TUI/web for a project.
// ---------------------------------------------------------------------------

/// Attached-client registry: which live sessions are bound to a workstream.
/// One row per (worker, project, tool) session — the stable `session_id` lets a
/// reconnecting CLI re-attach to the same row instead of piling up duplicates.
pub async fn ensure_client_schema(pg: &PgPool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS workstream_clients ( \
            id             uuid PRIMARY KEY DEFAULT gen_random_uuid(), \
            workstream_id  uuid NOT NULL, \
            session_id     text NOT NULL UNIQUE, \
            worker_name    text NOT NULL, \
            tool           text NOT NULL, \
            cwd            text, \
            goal           text, \
            status         text NOT NULL DEFAULT 'attached', \
            attached_at    timestamptz NOT NULL DEFAULT now(), \
            last_report_at timestamptz )",
    )
    .execute(pg)
    .await?;
    Ok(())
}

/// Canonicalize a git remote for equality (mirrors project_scope). Empty → None.
fn canon(remote: &str) -> Option<String> {
    if remote.trim().is_empty() {
        return None;
    }
    crate::project_scope::canonical_remote(remote)
}

/// Resolve the workstream a directory belongs to. Git-remote-first (canonical
/// match against every workstream's `git_remote`), then falls back to matching
/// the resolved project id / basename against `project_key`. `None` if nothing
/// matches — the caller reports that the dir isn't under a known project.
pub async fn workstream_for_dir(pg: &PgPool, cwd: &std::path::Path) -> Result<Option<Workstream>> {
    let resolved = crate::project_scope::resolve_from_dir(Some(cwd));
    let all = sqlx::query_as::<_, Workstream>(
        "SELECT id, project_key, git_remote, basename, working_summary, status \
           FROM ff_workstreams WHERE status = 'active'",
    )
    .fetch_all(pg)
    .await?;

    // 1. Canonical git-remote match (stable across clone paths + SSH aliases).
    if let Some(id) = resolved.as_deref()
        && let Some(want) = canon(id)
    {
        if let Some(ws) = all
            .iter()
            .find(|w| canon(&w.git_remote).as_deref() == Some(&want))
        {
            return Ok(Some(ws.clone()));
        }
    }
    // 2. Fallback: resolved id (or its `local:<base>` / basename) == project_key.
    if let Some(id) = resolved.as_deref() {
        let base = id.rsplit(['/', ':']).next().unwrap_or(id);
        if let Some(ws) = all
            .iter()
            .find(|w| w.project_key == id || w.project_key.eq_ignore_ascii_case(base))
        {
            return Ok(Some(ws.clone()));
        }
    }
    Ok(None)
}

/// Stable session id for a (worker, project, tool) triple — the same folder on
/// the same node with the same CLI always re-attaches to one row.
pub fn session_id_for(worker: &str, project_key: &str, tool: &str) -> String {
    format!("{worker}-{project_key}-{tool}")
}

/// Attach a client session to its project's workstream. Idempotent on
/// `session_id` (re-attach refreshes goal + attached_at). Returns the workstream
/// and the stable session id the client uses for subsequent `report` calls.
pub async fn attach(
    pg: &PgPool,
    ws: &Workstream,
    worker: &str,
    tool: &str,
    cwd: &str,
    goal: Option<&str>,
) -> Result<String> {
    ensure_client_schema(pg).await?;
    let sid = session_id_for(worker, &ws.project_key, tool);
    sqlx::query(
        "INSERT INTO workstream_clients \
            (workstream_id, session_id, worker_name, tool, cwd, goal, status, attached_at) \
         VALUES ($1,$2,$3,$4,$5,$6,'attached', now()) \
         ON CONFLICT (session_id) DO UPDATE SET \
            workstream_id = EXCLUDED.workstream_id, \
            worker_name   = EXCLUDED.worker_name, \
            cwd           = EXCLUDED.cwd, \
            goal          = COALESCE(EXCLUDED.goal, workstream_clients.goal), \
            status        = 'attached', \
            attached_at   = now()",
    )
    .bind(ws.id)
    .bind(&sid)
    .bind(worker)
    .bind(tool)
    .bind(cwd)
    .bind(goal)
    .execute(pg)
    .await?;
    Ok(sid)
}

/// Report working state from an attached session into its workstream: update
/// the shared `working_summary` / `focus` (what's happening now) and append a
/// timestamped note to `open_threads` (the running activity log). Any of the
/// three may be omitted. Bumps the client's `last_report_at` heartbeat.
pub async fn report(
    pg: &PgPool,
    session_id: &str,
    summary: Option<&str>,
    focus: Option<&str>,
    note: Option<&str>,
) -> Result<Workstream> {
    let ws_id: uuid::Uuid = sqlx::query_scalar(
        "UPDATE workstream_clients SET last_report_at = now() \
          WHERE session_id = $1 RETURNING workstream_id",
    )
    .bind(session_id)
    .fetch_optional(pg)
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!("session '{session_id}' is not attached — run `ff workstream attach` first")
    })?;

    // COALESCE keeps prior values when a field is omitted; the note is appended
    // to the open_threads jsonb array with a server timestamp + the session id.
    let ws = sqlx::query_as::<_, Workstream>(
        "UPDATE ff_workstreams SET \
            working_summary = COALESCE($2, working_summary), \
            focus           = COALESCE($3, focus), \
            open_threads    = CASE WHEN $4::text IS NULL THEN open_threads \
                ELSE COALESCE(open_threads, '[]'::jsonb) || \
                     jsonb_build_object('at', now(), 'session', $5::text, 'note', $4::text) END, \
            updated_at      = now() \
          WHERE id = $1 \
       RETURNING id, project_key, git_remote, basename, working_summary, status",
    )
    .bind(ws_id)
    .bind(summary)
    .bind(focus)
    .bind(note)
    .bind(session_id)
    .fetch_one(pg)
    .await?;
    Ok(ws)
}

/// An attached client, for status display.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AttachedClient {
    pub session_id: String,
    pub worker_name: String,
    pub tool: String,
    pub goal: Option<String>,
    pub status: String,
    pub last_report_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// List the sessions attached to a workstream, most-recently-active first.
pub async fn attached_clients(
    pg: &PgPool,
    workstream_id: uuid::Uuid,
) -> Result<Vec<AttachedClient>> {
    ensure_client_schema(pg).await?;
    let rows = sqlx::query_as::<_, AttachedClient>(
        "SELECT session_id, worker_name, tool, goal, status, last_report_at \
           FROM workstream_clients WHERE workstream_id = $1 \
          ORDER BY last_report_at DESC NULLS LAST, attached_at DESC",
    )
    .bind(workstream_id)
    .fetch_all(pg)
    .await?;
    Ok(rows)
}
