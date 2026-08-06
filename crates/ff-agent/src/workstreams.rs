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
use regex::Regex;
use sqlx::PgPool;
use std::sync::OnceLock;

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
            (id, project_id, project_key, git_remote, basename, aliases, goal, status, leader_generation, updated_at) \
         SELECT gen_random_uuid(), p.id, p.id, coalesce(p.repo_url, ''), \
                coalesce(nullif(p.display_name, ''), p.id), '{}'::jsonb, \
                coalesce(nullif(p.display_name, ''), p.id) || ' — project session-of-record', \
                'active', 0, now() \
           FROM projects p \
          WHERE p.status = 'active' \
         ON CONFLICT (project_key) DO UPDATE SET \
                project_id = EXCLUDED.project_id, \
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
    pub project_id: String,
    pub project_key: String,
    pub git_remote: Option<String>,
    pub basename: Option<String>,
    pub aliases: serde_json::Value,
    pub goal: Option<String>,
    pub working_summary: Option<String>,
    pub focus: Option<String>,
    pub open_threads: serde_json::Value,
    pub status: String,
    pub leader_generation: i32,
    pub owner_identity: String,
}

const WORKSTREAM_COLUMNS: &str = "id, project_id, project_key, git_remote, basename, aliases, \
    goal, working_summary, focus, open_threads, status, leader_generation, owner_identity";

/// Resolve the workstream for a project key (the session-of-record clients
/// attach to). `None` if the project has no workstream yet.
pub async fn workstream_for_project(pg: &PgPool, project_key: &str) -> Result<Option<Workstream>> {
    let all = sqlx::query_as::<_, Workstream>(&format!(
        "SELECT {WORKSTREAM_COLUMNS} FROM ff_workstreams WHERE status = 'active'"
    ))
    .fetch_all(pg)
    .await?;

    // Explicit aliases are operator overrides and therefore win even if the
    // supplied value also happens to be another row's derived project key.
    if let Some(ws) = all
        .iter()
        .find(|ws| alias_matches(&ws.aliases, project_key))
    {
        return Ok(Some(ws.clone()));
    }
    Ok(all
        .into_iter()
        .find(|ws| ws.project_id == project_key || ws.project_key == project_key))
}

fn alias_matches(aliases: &serde_json::Value, candidate: &str) -> bool {
    aliases.as_object().is_some_and(|aliases| {
        aliases.contains_key(candidate)
            || aliases
                .values()
                .any(|value| value.as_str() == Some(candidate))
    })
}

/// Attach a client session as an open workstream thread and publish its current
/// summary. Both writes are committed together so readers never observe a
/// newly attached session without its corresponding workstream state.
pub async fn attach_client_session(
    pg: &PgPool,
    workstream_id: uuid::Uuid,
    session_id: &str,
    _working_summary: &str,
) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query(
        "INSERT INTO workstream_threads \
            (workstream_id, label, claimed_by) \
         VALUES ($1, $2, $2)",
    )
    .bind(workstream_id)
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
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
    let identity = crate::project_scope::identity_from_dir(Some(cwd));
    let all = sqlx::query_as::<_, Workstream>(&format!(
        "SELECT {WORKSTREAM_COLUMNS} FROM ff_workstreams WHERE status = 'active'"
    ))
    .fetch_all(pg)
    .await?;

    let candidates = identity
        .iter()
        .flat_map(|identity| {
            [
                identity.explicit.as_deref(),
                identity.git_remote.as_deref(),
                identity.basename.as_deref(),
            ]
        })
        .flatten()
        .collect::<Vec<_>>();

    // Alias-map override wins over every derived identity.
    for ws in &all {
        if candidates
            .iter()
            .any(|candidate| alias_matches(&ws.aliases, candidate))
        {
            return Ok(Some(ws.clone()));
        }
    }

    // 1. Canonical git-remote match (stable across clone paths + SSH aliases).
    if let Some(id) = identity
        .as_ref()
        .and_then(|identity| identity.git_remote.as_deref())
        && let Some(want) = canon(id)
    {
        if let Some(ws) = all
            .iter()
            .find(|w| w.git_remote.as_deref().and_then(canon).as_deref() == Some(&want))
        {
            return Ok(Some(ws.clone()));
        }
    }
    // 2. Fallback: resolved id (or its `local:<base>` / basename) == project_key.
    if let Some(id) = resolved.as_deref() {
        let base = id.rsplit(['/', ':']).next().unwrap_or(id);
        if let Some(ws) = all.iter().find(|w| {
            w.project_id == id
                || w.project_key == id
                || w.project_id.eq_ignore_ascii_case(base)
                || w.basename
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(base))
        }) {
            return Ok(Some(ws.clone()));
        }
    }
    Ok(None)
}

/// Attach an authenticated fleet operator and refresh durable presence.
pub async fn attach_operator(pg: &PgPool, ws: &Workstream, operator_identity: &str) -> Result<()> {
    authorize_operator(ws, operator_identity)?;
    sqlx::query(
        "INSERT INTO session_attachments \
            (workstream_id, operator_identity, attached_at, last_seen_at) \
         VALUES ($1, $2, now(), now()) \
         ON CONFLICT (workstream_id, operator_identity) DO UPDATE \
             SET last_seen_at = now()",
    )
    .bind(ws.id)
    .bind(operator_identity)
    .execute(pg)
    .await?;
    Ok(())
}

/// Enforce owner scoping for read-only and mutating session operations.
pub fn authorize_operator(ws: &Workstream, operator_identity: &str) -> Result<()> {
    if operator_identity.trim().is_empty() || operator_identity != ws.owner_identity {
        anyhow::bail!("operator fleet identity is not authorized for this workstream");
    }
    Ok(())
}

/// Append a redacted note with leader-assigned monotonic causal sequence.
pub async fn append_note(
    pg: &PgPool,
    ws: &Workstream,
    operator_identity: &str,
    note: &str,
) -> Result<i64> {
    attach_operator(pg, ws, operator_identity).await?;
    let redacted = redact_secrets(note);
    let source_sha256 = sha256_hex(redacted.as_bytes());
    let mut tx = pg.begin().await?;
    let seq: i64 = sqlx::query_scalar(
        "UPDATE ff_workstreams SET next_seq = next_seq + 1, updated_at = now() \
         WHERE id = $1 AND owner_identity = $2 RETURNING next_seq",
    )
    .bind(ws.id)
    .bind(operator_identity)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO workstream_notes \
            (workstream_id, seq, note, source_sha256, created_by) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(ws.id)
    .bind(seq)
    .bind(redacted)
    .bind(source_sha256)
    .bind(operator_identity)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(seq)
}

/// Redact common credential forms before content leaves the node.
pub fn redact_secrets(input: &str) -> String {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r#"(?i)["']?\b(?:password|token|secret|api[_-]?key)["']?\s*[:=]\s*["']?[^\s,;"'}]+"#,
            r"(?i)\b(?:authorization\s*:\s*)?bearer\s+[A-Za-z0-9._~+/=-]+",
            r"\bghp_[A-Za-z0-9_]+",
            r"\bgithub_pat_[A-Za-z0-9_]+",
            r"\bsk-[A-Za-z0-9_-]+",
            r"\bAGE-SECRET-KEY-[A-Z0-9-]+",
            r"\bops_[A-Za-z0-9_-]+",
            r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("valid workstream redaction regex"))
        .collect()
    });
    patterns.iter().fold(input.to_owned(), |text, pattern| {
        pattern.replace_all(&text, "[REDACTED]").into_owned()
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Stable session id for a (worker, project, tool) triple — the same folder on
/// the same node with the same CLI always re-attaches to one row.
///
/// See [`session_id_for_token`] for the multi-session form. Kept for callers that
/// only need the single-seat identity (e.g. a legacy re-attach).
pub fn session_id_for(worker: &str, project_key: &str, tool: &str) -> String {
    session_id_for_token(worker, project_key, tool, None)
}

/// Session id for a (worker, project, tool) plus an optional per-session `token`
/// — the CLI's OWN native session UUID (Claude/Codex/Kimi each expose one).
///
/// With a token, TWO sessions of the SAME tool on the SAME node+project get
/// DISTINCT rows (`…-{tool}-{uuid}`), so an operator can run 2 codex + 1 kimi + 3
/// claude sessions on one repo and each is its own seat in `workstream_clients`
/// (2026-07-28). Without a token it falls back to the bare `…-{tool}` triple
/// (one seat per tool — backward compatible). The token is trimmed and any
/// characters outside `[A-Za-z0-9._-]` are replaced with `_` so a stray native
/// id can't break the composite key.
pub fn session_id_for_token(
    worker: &str,
    project_key: &str,
    tool: &str,
    token: Option<&str>,
) -> String {
    let base = format!("{worker}-{project_key}-{tool}");
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => {
            let clean: String = t
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!("{base}-{clean}")
        }
        None => base,
    }
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
    session_token: Option<&str>,
) -> Result<String> {
    ensure_client_schema(pg).await?;
    let sid = session_id_for_token(worker, &ws.project_key, tool, session_token);
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
/// `focus` (what the session is doing now) and append a timestamped note to
/// `open_threads` (the running activity log). The leader exclusively derives
/// `working_summary`; supplying one is rejected. Bumps the client's
/// `last_report_at` heartbeat.
pub async fn report(
    pg: &PgPool,
    session_id: &str,
    summary: Option<&str>,
    focus: Option<&str>,
    note: Option<&str>,
) -> Result<Workstream> {
    if summary.is_some() {
        anyhow::bail!("working_summary is leader-owned; clients may report focus or notes only");
    }
    let redacted_focus = focus.map(redact_secrets);
    let redacted_note = note.map(redact_secrets);
    let ws_id: uuid::Uuid = sqlx::query_scalar(
        "UPDATE workstream_clients SET last_report_at = now(), status = 'attached' \
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
    let ws = sqlx::query_as::<_, Workstream>(&format!(
        "UPDATE ff_workstreams SET \
            focus           = COALESCE($2, focus), \
            open_threads    = CASE WHEN $3::text IS NULL THEN open_threads \
                ELSE COALESCE(open_threads, '[]'::jsonb) || \
                     jsonb_build_object('at', now(), 'session', $4::text, 'note', $3::text) END, \
            updated_at      = now() \
          WHERE id = $1 \
       RETURNING {WORKSTREAM_COLUMNS}"
    ))
    .bind(ws_id)
    .bind(redacted_focus.as_deref())
    .bind(redacted_note.as_deref())
    .bind(session_id)
    .fetch_one(pg)
    .await?;
    Ok(ws)
}

type WorkItemSummaryCounts = (i64, i64, i64, Option<String>);

const WORK_ITEM_SUMMARY_UNAVAILABLE: &str = "work-item state unavailable (auto)";

fn mechanical_working_summary(counts: Option<WorkItemSummaryCounts>) -> String {
    let Some((building, merged_1h, failed, latest)) = counts else {
        return WORK_ITEM_SUMMARY_UNAVAILABLE.to_string();
    };

    let mut summary = format!("{building} building · {merged_1h} merged/1h · {failed} failed");
    if let Some(title) = latest.filter(|title| !title.trim().is_empty()) {
        summary.push_str(&format!(" · latest: {title}"));
    }
    summary.push_str(" (auto)");
    summary
}

/// Auto-derive each project's workstream `working_summary` from live work-item
/// activity. The leader is the sole summary writer; attached sessions publish
/// semantic narrative through `focus` and `open_threads` instead. Runs on the
/// leader tick and returns how many summaries actually changed.
pub async fn derive_working_summaries(pg: &PgPool) -> Result<u64> {
    let projects = sqlx::query_scalar::<_, String>(
        "SELECT project_key FROM ff_workstreams WHERE status = 'active'",
    )
    .fetch_all(pg)
    .await?;

    let mut updated = 0u64;
    for project in projects {
        let counts = sqlx::query_as::<_, WorkItemSummaryCounts>(
            "SELECT \
               (SELECT count(*) FROM work_items WHERE project_id = $1 AND status = 'building'), \
               (SELECT count(*) FROM work_item_provenance p JOIN work_items w ON w.id = p.work_item_id \
                 WHERE w.project_id = $1 AND p.merged_at > now() - interval '1 hour'), \
               (SELECT count(*) FROM work_items WHERE project_id = $1 AND status = 'failed'), \
               (SELECT left(w.title, 48) FROM work_items w \
                 WHERE w.project_id = $1 AND w.status = 'building' \
                 ORDER BY COALESCE(w.completed_at, w.started_at, w.created_at) DESC NULLS LAST \
                 LIMIT 1)",
        )
        .bind(&project)
        .fetch_one(pg)
        .await;
        let summary = match counts {
            Ok(counts) => mechanical_working_summary(Some(counts)),
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    %error,
                    "workstreams: work-item summary source unavailable"
                );
                mechanical_working_summary(None)
            }
        };

        let n = sqlx::query(
            "UPDATE ff_workstreams SET working_summary = $2, updated_at = now() \
              WHERE project_key = $1 AND working_summary IS DISTINCT FROM $2",
        )
        .bind(&project)
        .bind(&summary)
        .execute(pg)
        .await?
        .rows_affected();
        updated += n;
    }
    Ok(updated)
}

/// Lightweight liveness ping from an attached session — bumps `last_report_at`
/// WITHOUT touching the shared summary/focus. Called from a session Stop hook so
/// the workstream knows the client is still alive even between substantive
/// `report` calls. Also revives a pruned (detached) seat: any sign of life means
/// the session is real. Silent no-op if the session isn't attached (the
/// SessionStart hook attaches; a Stop firing before attach shouldn't error).
pub async fn heartbeat(pg: &PgPool, session_id: &str) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE workstream_clients SET last_report_at = now(), status = 'attached' \
          WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(pg)
    .await?
    .rows_affected();
    Ok(n > 0)
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
/// Detached (pruned) seats are hidden — re-attach or any heartbeat revives one.
pub async fn attached_clients(
    pg: &PgPool,
    workstream_id: uuid::Uuid,
) -> Result<Vec<AttachedClient>> {
    ensure_client_schema(pg).await?;
    let rows = sqlx::query_as::<_, AttachedClient>(
        "SELECT session_id, worker_name, tool, goal, status, last_report_at \
           FROM workstream_clients WHERE workstream_id = $1 AND status = 'attached' \
          ORDER BY last_report_at DESC NULLS LAST, attached_at DESC",
    )
    .bind(workstream_id)
    .fetch_all(pg)
    .await?;
    Ok(rows)
}

/// Mark a single session's workstream seat as detached.
pub async fn detach(pg: &PgPool, session_id: &str) -> Result<bool> {
    ensure_client_schema(pg).await?;
    let result =
        sqlx::query("UPDATE workstream_clients SET status = 'detached' WHERE session_id = $1")
            .bind(session_id)
            .execute(pg)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// A seat pruned by `prune_stale_clients`, for reporting what changed.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PrunedClient {
    pub session_id: String,
    pub worker_name: String,
    pub tool: String,
    pub last_active_at: chrono::DateTime<chrono::Utc>,
}

/// Mark seats with no activity since `cutoff` as detached (soft prune — the row
/// stays for history; `attach`/`report`/`heartbeat` revive it on any sign of
/// life). A seat's activity is `last_report_at`, falling back to `attached_at`
/// for sessions that never reported. `dry_run` returns what WOULD be pruned
/// without writing. Returns the affected seats, most-stale first.
pub async fn prune_stale_clients(
    pg: &PgPool,
    workstream_id: uuid::Uuid,
    cutoff: chrono::DateTime<chrono::Utc>,
    dry_run: bool,
) -> Result<Vec<PrunedClient>> {
    ensure_client_schema(pg).await?;
    let rows = if dry_run {
        sqlx::query_as::<_, PrunedClient>(
            "SELECT session_id, worker_name, tool, \
                    COALESCE(last_report_at, attached_at) AS last_active_at \
               FROM workstream_clients \
              WHERE workstream_id = $1 AND status = 'attached' \
                AND COALESCE(last_report_at, attached_at) < $2 \
              ORDER BY last_active_at",
        )
        .bind(workstream_id)
        .bind(cutoff)
        .fetch_all(pg)
        .await?
    } else {
        sqlx::query_as::<_, PrunedClient>(
            "UPDATE workstream_clients SET status = 'detached' \
              WHERE workstream_id = $1 AND status = 'attached' \
                AND COALESCE(last_report_at, attached_at) < $2 \
            RETURNING session_id, worker_name, tool, \
                    COALESCE(last_report_at, attached_at) AS last_active_at",
        )
        .bind(workstream_id)
        .bind(cutoff)
        .fetch_all(pg)
        .await?
    };
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_without_token_is_the_bare_triple() {
        // Backward compatible: one seat per (worker, project, tool).
        assert_eq!(
            session_id_for_token("adele", "forge-fleet", "kimi", None),
            "adele-forge-fleet-kimi"
        );
        assert_eq!(
            session_id_for("adele", "forge-fleet", "kimi"),
            "adele-forge-fleet-kimi"
        );
        // Empty/whitespace token collapses to the bare triple (no trailing dash).
        assert_eq!(
            session_id_for_token("adele", "forge-fleet", "kimi", Some("   ")),
            "adele-forge-fleet-kimi"
        );
    }

    #[test]
    fn distinct_tokens_yield_distinct_seats_for_same_tool() {
        // The whole point: two kimi sessions on one repo → two rows.
        let a = session_id_for_token("adele", "forge-fleet", "kimi", Some("session_a2aaef27"));
        let b = session_id_for_token("adele", "forge-fleet", "kimi", Some("session_9bcd1234"));
        assert_ne!(a, b);
        assert_eq!(a, "adele-forge-fleet-kimi-session_a2aaef27");
    }

    #[test]
    fn token_is_sanitized_so_a_stray_id_cant_break_the_key() {
        // Spaces/slashes/colons in a native id are replaced with '_'.
        assert_eq!(
            session_id_for_token("adele", "forge-fleet", "codex", Some("a b/c:d")),
            "adele-forge-fleet-codex-a_b_c_d"
        );
    }

    fn test_workstream() -> Workstream {
        Workstream {
            id: uuid::Uuid::nil(),
            project_id: "hireflow360".to_string(),
            project_key: "hireflow360".to_string(),
            git_remote: None,
            basename: Some("hireflow360".to_string()),
            aliases: serde_json::json!({
                "github.com/acme/hireflow": true,
                "legacy-hireflow": "hireflow360"
            }),
            goal: None,
            working_summary: None,
            focus: None,
            open_threads: serde_json::json!([]),
            status: "active".to_string(),
            leader_generation: 0,
            owner_identity: "operator:acme".to_string(),
        }
    }

    #[test]
    fn aliases_match_keys_and_values() {
        let ws = test_workstream();
        assert!(alias_matches(&ws.aliases, "github.com/acme/hireflow"));
        assert!(alias_matches(&ws.aliases, "hireflow360"));
        assert!(!alias_matches(&ws.aliases, "another-project"));
    }

    #[test]
    fn workstream_reads_are_owner_scoped() {
        let ws = test_workstream();
        assert!(authorize_operator(&ws, "operator:acme").is_ok());
        assert!(authorize_operator(&ws, "operator:other").is_err());
        assert!(authorize_operator(&ws, "").is_err());
    }

    #[test]
    fn notes_are_redacted_before_hash_or_persistence() {
        let note = "deploy token=abc sk-live-secret ghp_private password=hunter2 safely";
        let redacted = redact_secrets(note);
        assert_eq!(
            redacted,
            "deploy [REDACTED] [REDACTED] [REDACTED] [REDACTED] safely"
        );
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("hunter2"));
    }

    #[test]
    fn source_redaction_handles_structured_and_multiline_secrets() {
        let note = "headers:\nAuthorization: Bearer abc.def-123\n\
                    payload={\"api_key\":\"top-secret\"}\n\
                    jwt=eyJabc.def.ghi";
        let redacted = redact_secrets(note);
        assert_eq!(redacted.lines().count(), note.lines().count());
        for secret in ["abc.def-123", "top-secret", "eyJabc.def.ghi"] {
            assert!(!redacted.contains(secret), "secret leaked: {secret}");
        }
    }

    #[test]
    fn mechanical_summary_is_deterministic_for_zero_and_unavailable_counts() {
        assert_eq!(
            mechanical_working_summary(Some((0, 0, 0, None))),
            "0 building · 0 merged/1h · 0 failed (auto)"
        );
        assert_eq!(
            mechanical_working_summary(None),
            "work-item state unavailable (auto)"
        );
    }

    #[test]
    fn mechanical_summary_includes_only_a_nonempty_latest_title() {
        assert_eq!(
            mechanical_working_summary(Some((2, 3, 4, Some("truth repair".to_string())))),
            "2 building · 3 merged/1h · 4 failed · latest: truth repair (auto)"
        );
        assert_eq!(
            mechanical_working_summary(Some((2, 3, 4, Some("   ".to_string())))),
            "2 building · 3 merged/1h · 4 failed (auto)"
        );
    }

    #[tokio::test]
    async fn client_summary_is_rejected_before_any_database_access() {
        let pg = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgresql://invalid.invalid/forgefleet")
            .expect("lazy pool");
        let error = report(&pg, "session", Some("rolling-deploy ON"), None, None)
            .await
            .expect_err("clients must not write leader-owned summaries");
        assert!(
            error
                .to_string()
                .contains("working_summary is leader-owned")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; self-verify runs this explicitly when configured"]
    async fn leader_summary_truth_is_fail_closed_and_client_narrative_stays_separate(pg: PgPool) {
        const WORKSTREAM_ID: &str = "00000000-0000-0000-0000-000000000101";
        const OLDER_WORK_ITEM_ID: &str = "00000000-0000-0000-0000-000000000102";
        const LATEST_WORK_ITEM_ID: &str = "00000000-0000-0000-0000-000000000103";
        const SESSION_ID: &str = "fresh-client";

        sqlx::raw_sql(
            "CREATE TABLE ff_workstreams (\
                 id uuid PRIMARY KEY, project_id text NOT NULL, project_key text NOT NULL UNIQUE,\
                 git_remote text, basename text, aliases jsonb NOT NULL DEFAULT '{}'::jsonb,\
                 goal text, working_summary text, focus text,\
                 open_threads jsonb NOT NULL DEFAULT '[]'::jsonb,\
                 status text NOT NULL DEFAULT 'active', leader_generation integer NOT NULL DEFAULT 0,\
                 owner_identity text NOT NULL DEFAULT 'operator:fleet',\
                 updated_at timestamptz NOT NULL DEFAULT now());\
             CREATE TABLE workstream_clients (\
                 id bigserial PRIMARY KEY, workstream_id uuid NOT NULL,\
                 session_id text NOT NULL UNIQUE, worker_name text NOT NULL, tool text NOT NULL,\
                 cwd text, goal text, status text NOT NULL DEFAULT 'attached',\
                 attached_at timestamptz NOT NULL DEFAULT now(), last_report_at timestamptz);\
             CREATE TABLE work_items (\
                 id uuid PRIMARY KEY, project_id text NOT NULL, title text NOT NULL,\
                 status text NOT NULL, created_at timestamptz NOT NULL,\
                 started_at timestamptz, completed_at timestamptz);\
             CREATE TABLE work_item_provenance (\
                 work_item_id uuid NOT NULL, merged_at timestamptz);",
        )
        .execute(&pg)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ff_workstreams (id, project_id, project_key, basename, working_summary, focus)\
             VALUES ($1::uuid, 'forge-fleet', 'forge-fleet', 'forge-fleet',\
                     '11 fixes shipped + rolling-deploy ON', 'existing focus')",
        )
        .bind(WORKSTREAM_ID)
        .execute(&pg)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workstream_clients\
                 (workstream_id, session_id, worker_name, tool, last_report_at)\
             VALUES ($1::uuid, $2, 'adele', 'codex', now())",
        )
        .bind(WORKSTREAM_ID)
        .bind(SESSION_ID)
        .execute(&pg)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO work_items\
                 (id, project_id, title, status, created_at, started_at, completed_at)\
             VALUES\
                 ($1::uuid, 'forge-fleet', 'older by lifecycle', 'building',\
                  now() + interval '1 day', now() - interval '2 hours', NULL),\
                 ($2::uuid, 'forge-fleet', 'truth repair', 'building',\
                  now() - interval '1 day', now() - interval '1 hour', NULL)",
        )
        .bind(OLDER_WORK_ITEM_ID)
        .bind(LATEST_WORK_ITEM_ID)
        .execute(&pg)
        .await
        .unwrap();

        assert_eq!(derive_working_summaries(&pg).await.unwrap(), 1);
        let summary: String = sqlx::query_scalar(
            "SELECT working_summary FROM ff_workstreams WHERE project_key = 'forge-fleet'",
        )
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(
            summary,
            "2 building · 0 merged/1h · 0 failed · latest: truth repair (auto)"
        );

        sqlx::query(
            "UPDATE work_items SET completed_at = now() \
             WHERE id = $1::uuid",
        )
        .bind(OLDER_WORK_ITEM_ID)
        .execute(&pg)
        .await
        .unwrap();
        assert_eq!(derive_working_summaries(&pg).await.unwrap(), 1);
        let completed_summary: String = sqlx::query_scalar(
            "SELECT working_summary FROM ff_workstreams WHERE project_key = 'forge-fleet'",
        )
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(
            completed_summary,
            "2 building · 0 merged/1h · 0 failed · latest: older by lifecycle (auto)"
        );

        sqlx::query("DELETE FROM work_items")
            .execute(&pg)
            .await
            .unwrap();
        assert_eq!(derive_working_summaries(&pg).await.unwrap(), 1);
        let zero_summary: String = sqlx::query_scalar(
            "SELECT working_summary FROM ff_workstreams WHERE project_key = 'forge-fleet'",
        )
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(zero_summary, "0 building · 0 merged/1h · 0 failed (auto)");
        assert_eq!(derive_working_summaries(&pg).await.unwrap(), 0);

        sqlx::query("DROP TABLE work_item_provenance")
            .execute(&pg)
            .await
            .unwrap();
        assert_eq!(derive_working_summaries(&pg).await.unwrap(), 1);
        let unavailable: String = sqlx::query_scalar(
            "SELECT working_summary FROM ff_workstreams WHERE project_key = 'forge-fleet'",
        )
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(unavailable, "work-item state unavailable (auto)");

        let error = report(&pg, SESSION_ID, Some("rolling-deploy ON"), None, None)
            .await
            .expect_err("client summary must remain leader-owned");
        assert!(
            error
                .to_string()
                .contains("working_summary is leader-owned")
        );

        let updated = report(
            &pg,
            SESSION_ID,
            None,
            Some("diagnosing truth"),
            Some("mechanical summary repaired"),
        )
        .await
        .unwrap();
        assert_eq!(
            updated.working_summary.as_deref(),
            Some(unavailable.as_str())
        );
        assert_eq!(updated.focus.as_deref(), Some("diagnosing truth"));
        let thread = updated.open_threads.as_array().unwrap().last().unwrap();
        assert_eq!(thread["session"], SESSION_ID);
        assert_eq!(thread["note"], "mechanical summary repaired");
    }
}
