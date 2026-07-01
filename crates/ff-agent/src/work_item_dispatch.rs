//! Pillar 4 work_item dispatch.
//!
//! Leader-gated, host-scoped executor for work_items already assigned by
//! `work_item_scheduler`: find this host's busy sub-agent slots with an active
//! lease, create an isolated git worktree, run `ff cli codex`, heartbeat the
//! lease, push a branch, open a PR, enqueue merge, then free the slot.

use anyhow::{Context, Result, anyhow, bail};
use sqlx::{PgPool, Row};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tracing::{info, warn};
use uuid::Uuid;

use crate::sub_agents::ensure_workspaces;

/// How often the dispatch loop bumps a work_item lease's `heartbeat_at` while a
/// build runs. The lease reapers (`lease_takeover`, `work_item_scheduler`) MUST
/// use a stale window comfortably larger than this, or they'd reclaim a live
/// lease mid-build (the #589/#590 reaper bug class). `pub(crate)` so those
/// reapers' regression tests can assert the coupling.
pub(crate) const HEARTBEAT_SECS: u64 = 45;
const COMMAND_POLL_MS: u64 = 250;
// 18 min. codex reliably WRITES a complete diff in ~5-8 min but then often fails
// to EXIT, running until the timeout (dogfooded 2026-06-30/07-01). Since the
// dispatch now SALVAGES the worktree diff on timeout (worktree_has_diff →
// commit → PR), we no longer need a long budget for codex to "finish" — we just
// need enough time for it to write. 18 min gives comfortable headroom over the
// ~8-min write, then salvages, instead of wasting a slot for 45 min on a
// non-terminating process. A genuinely longer task simply salvages whatever diff
// exists at 18 min (CI verifies it). Followers are 20-core/121GB.
const FF_TIMEOUT_SECS: u64 = 1080;
const MAX_DISPATCH_PER_TICK: i64 = 1;

#[derive(Debug, Clone)]
struct AssignedWorkItem {
    work_item_id: Uuid,
    project_id: String,
    title: String,
    description: Option<String>,
    base_branch: Option<String>,
    repo_path: PathBuf,
    sub_agent_id: Uuid,
    computer_id: Uuid,
    slot: i32,
}

#[derive(Debug, Clone)]
struct WorktreeRecord {
    worktree_path: PathBuf,
    base_branch: String,
    task_branch: String,
}

/// One dispatch pass. Returns the number of work_items started by this host.
pub async fn evaluate_work_item_dispatch(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let repo_path = std::env::current_dir().context("resolve current repo path")?;
    let assigned = assigned_work_items(pg, worker_name, &repo_path, MAX_DISPATCH_PER_TICK).await?;
    if assigned.is_empty() {
        return Ok(0);
    }

    let max_slot = assigned.iter().map(|w| w.slot).max().unwrap_or(0).max(0) as u32;
    ensure_workspaces(max_slot + 1).map_err(|e| anyhow!("ensure sub-agent workspaces: {e}"))?;

    let mut started = 0usize;
    for item in assigned {
        match dispatch_one(pg.clone(), item.clone(), worker_name.to_string()).await {
            Ok(()) => started += 1,
            Err(e) => {
                warn!(
                    work_item_id = %item.work_item_id,
                    sub_agent_id = %item.sub_agent_id,
                    error = %e,
                    "work_item_dispatch: dispatch failed"
                );
                if let Err(cleanup) = mark_failed_and_release(pg, &item, &e.to_string()).await {
                    warn!(
                        work_item_id = %item.work_item_id,
                        error = %cleanup,
                        "work_item_dispatch: failure cleanup failed"
                    );
                }
            }
        }
    }

    if started > 0 {
        info!(started, "work_item_dispatch: started assigned work_items");
    }
    Ok(started)
}

/// Spawn the dispatch loop. PER-HOST (not leader-gated): the scheduler (leader)
/// assigns work_items to slots on ANY host, and each host must execute ITS OWN
/// slots. `evaluate_work_item_dispatch` is host-scoped (`c.name = worker_name`),
/// so running it on every host dispatches only that host's assigned slots.
pub fn spawn_work_item_dispatch(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = evaluate_work_item_dispatch(&pg, &worker_name).await {
                        warn!(error = %e, "work_item_dispatch tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work_item_dispatch loop stopped");
    })
}

/// Expand a leading `~` to $HOME (computers.source_tree_path is stored as
/// `~/projects/forge-fleet` etc.). Leaves absolute paths untouched.
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    p.to_string()
}

async fn assigned_work_items(
    pg: &PgPool,
    worker_name: &str,
    default_repo_path: &Path,
    limit: i64,
) -> Result<Vec<AssignedWorkItem>> {
    let default_repo_path = default_repo_path.to_string_lossy().to_string();
    let rows = sqlx::query(
        r#"
        SELECT
            w.id AS work_item_id,
            w.project_id,
            w.title,
            w.description,
            w.base_branch,
            -- Build path resolution (per-project, V141 project_folders): an
            -- explicit metadata override wins; else this project's local folder
            -- on THIS host (host-specific row preferred, then a canonical
            -- computer_id-NULL row); else the host's source_tree_path (correct
            -- only for forge-fleet itself); else the daemon's cwd. Without the
            -- project_folders lookup a non-forge-fleet work_item (e.g. a
            -- hireflow360 port) worktree'd against the host's forge-fleet tree —
            -- the wrong repo (operator-reported 2026-06-20). forge-fleet has no
            -- project_folders rows, so it still falls through to source_tree_path
            -- exactly as before (backward-compatible).
            COALESCE(
                NULLIF(w.metadata->>'repo_path', ''),
                (SELECT pf.path
                   FROM project_folders pf
                  WHERE pf.project_id = w.project_id
                    AND (pf.computer_id = c.id OR pf.computer_id IS NULL)
                  ORDER BY CASE WHEN pf.computer_id = c.id THEN 0
                                WHEN pf.computer_id IS NULL THEN 1
                                ELSE 2 END,
                           pf.is_primary DESC,
                           pf.created_at ASC
                  LIMIT 1),
                NULLIF(c.source_tree_path, ''),
                $2
            ) AS repo_path,
            sa.id AS sub_agent_id,
            sa.computer_id,
            sa.slot
          FROM sub_agents sa
          JOIN computers c ON c.id = sa.computer_id
          JOIN work_items w ON w.id = sa.current_work_item_id
          JOIN work_item_leases l
            ON l.work_item_id = w.id
           AND l.sub_agent_id = sa.id
           AND l.released_at IS NULL
         WHERE c.name = $1
           AND sa.status = 'busy'
           AND sa.current_work_item_id IS NOT NULL
           AND w.status = 'claimed'
           AND NOT EXISTS (
               SELECT 1
                 FROM work_item_worktrees wt
                WHERE wt.work_item_id = w.id
                  AND wt.status IN ('creating', 'active', 'ready_for_review')
           )
         ORDER BY l.created_at ASC
         LIMIT $3
        "#,
    )
    .bind(worker_name)
    .bind(default_repo_path)
    .bind(limit)
    .fetch_all(pg)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(AssignedWorkItem {
                work_item_id: r.get("work_item_id"),
                project_id: r.get("project_id"),
                title: r.get("title"),
                description: r.try_get("description").ok().flatten(),
                base_branch: r.try_get("base_branch").ok().flatten(),
                repo_path: PathBuf::from(expand_tilde(&r.get::<String, _>("repo_path"))),
                sub_agent_id: r.get("sub_agent_id"),
                computer_id: r.get("computer_id"),
                slot: r.get("slot"),
            })
        })
        .collect()
}

async fn ensure_repo_checked_out(pg: &PgPool, item: &AssignedWorkItem) -> Result<()> {
    if item.repo_path.exists() && item.repo_path.join(".git").exists() {
        return Ok(());
    }

    let github_url: Option<String> = sqlx::query_scalar(
        "SELECT github_url
           FROM project_repos
          WHERE project_id = $1
            AND is_primary = TRUE
            AND NULLIF(github_url, '') IS NOT NULL
          LIMIT 1",
    )
    .bind(&item.project_id)
    .fetch_optional(pg)
    .await
    .with_context(|| format!("lookup primary repo for project {}", item.project_id))?;

    let github_url = github_url.ok_or_else(|| {
        anyhow!(
            "repo path {} is not a git repo and project {} has no primary project_repos github_url",
            item.repo_path.display(),
            item.project_id
        )
    })?;

    if let Some(parent) = item
        .repo_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create repo parent {}", parent.display()))?;
    }

    info!(
        work_item_id = %item.work_item_id,
        project_id = %item.project_id,
        repo_path = %item.repo_path.display(),
        github_url = %github_url,
        "work_item_dispatch: cloning project repo"
    );

    let repo_path = item.repo_path.clone();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("git");
        cmd.arg("clone").arg(&github_url).arg(&repo_path);
        run_command_timeout(cmd, Duration::from_secs(600))
    })
    .await
    .context("join git clone task")?
    .with_context(|| format!("clone project repo into {}", item.repo_path.display()))?;

    Ok(())
}

async fn dispatch_one(pg: PgPool, item: AssignedWorkItem, worker_name: String) -> Result<()> {
    ensure_repo_checked_out(&pg, &item).await?;
    let worktree = create_worktree_for_item(&pg, &item).await?;
    mark_building(&pg, &item).await?;

    let (stop_heartbeat_tx, stop_heartbeat_rx) = watch::channel(false);
    let heartbeat = spawn_heartbeat(pg.clone(), item.work_item_id, stop_heartbeat_rx);

    let started = std::time::Instant::now();
    let dispatch_full = run_ff_dispatch(&pg, &item, &worktree).await;
    let _ = stop_heartbeat_tx.send(true);
    let _ = heartbeat.await;

    // Split (backend, output) into the backend used + a plain Result<Output> for
    // the existing consumers. On error, no backend is carried, so use the
    // best-effort primary (for training attribution).
    let (backend_used, dispatch_result): (String, Result<Output>) = match dispatch_full {
        Ok((b, out)) => (b, Ok(out)),
        Err(e) => (
            primary_dispatch_backend(&pg, item.computer_id).await,
            Err(e),
        ),
    };

    // Capture the dispatch I/O in ff_interactions (training data) — `ff cli` is a
    // pass-through that doesn't log itself, so the dispatch records its own turn.
    record_dispatch_interaction(
        &pg,
        &item,
        &worker_name,
        &backend_used,
        &dispatch_result,
        started.elapsed(),
    )
    .await;

    match dispatch_result {
        Ok(output) => {
            info!(
                work_item_id = %item.work_item_id,
                stdout_len = output.stdout.len(),
                stderr_len = output.stderr.len(),
                "work_item_dispatch: ff dispatch completed"
            );
        }
        Err(e) => {
            mark_worktree_failed(&pg, item.work_item_id, &e.to_string()).await?;
            remove_worktree(&item.repo_path, &worktree.worktree_path)?;
            return Err(e);
        }
    }

    // codex (and most CLI agents) EDIT files but don't `git commit`. Commit any
    // changes it made in the worktree so they can become a PR. A clean worktree
    // (agent made no change) commits nothing → handled as "no commits" below.
    let dirty = commit_worktree_changes(&worktree.worktree_path, &item.title)?;
    info!(
        work_item_id = %item.work_item_id, dirty,
        "work_item_dispatch: committed agent changes (dirty={dirty})"
    );

    let has_commits = branch_has_commits(
        &item.repo_path,
        &worktree.base_branch,
        &worktree.task_branch,
    )?;
    if !has_commits {
        mark_completed_without_pr(&pg, &item).await?;
        mark_worktree_cleaned(&pg, item.work_item_id).await?;
        remove_worktree(&item.repo_path, &worktree.worktree_path)?;
        return Ok(());
    }

    let head_sha = git_head_sha(&worktree.worktree_path)?;
    push_branch(&item.repo_path, &worktree.task_branch)?;
    let pr_url = create_pr(&worktree.worktree_path, &item, &worktree).await?;

    mark_ready_for_review(&pg, &item, &worktree, &head_sha, &pr_url).await?;
    Ok(())
}

fn spawn_heartbeat(
    pg: PgPool,
    work_item_id: Uuid,
    mut stop_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = ff_db::pg_heartbeat_work_item_lease(&pg, work_item_id).await {
                        warn!(
                            work_item_id = %work_item_id,
                            error = %e,
                            "work_item_dispatch: lease heartbeat failed"
                        );
                    }
                }
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

async fn create_worktree_for_item(pg: &PgPool, item: &AssignedWorkItem) -> Result<WorktreeRecord> {
    let base_branch = match item.base_branch.as_deref() {
        Some(branch) if !branch.trim().is_empty() => branch.trim().to_string(),
        _ => default_branch(&item.repo_path).unwrap_or_else(|_| "main".to_string()),
    };
    let short_id = item.work_item_id.simple().to_string()[..12].to_string();
    let task_branch = format!("wi/{short_id}");

    let workspaces = ensure_workspaces((item.slot.max(0) as u32) + 1)
        .map_err(|e| anyhow!("ensure sub-agent workspaces: {e}"))?;
    let slot_ws = workspaces
        .get(item.slot.max(0) as usize)
        .cloned()
        .ok_or_else(|| anyhow!("missing workspace for slot {}", item.slot))?;
    let worktree_path = slot_ws.join("worktrees").join(&task_branch);
    std::fs::create_dir_all(
        worktree_path
            .parent()
            .ok_or_else(|| anyhow!("worktree path has no parent: {}", worktree_path.display()))?,
    )
    .with_context(|| format!("create worktree parent for {}", worktree_path.display()))?;

    insert_worktree_creating(pg, item, &worktree_path, &base_branch, &task_branch).await?;

    if worktree_path.exists() {
        remove_worktree(&item.repo_path, &worktree_path)
            .with_context(|| format!("remove stale worktree {}", worktree_path.display()))?;
    }

    match git_worktree_add(&item.repo_path, &worktree_path, &base_branch, &task_branch) {
        Ok(()) => {
            sqlx::query(
                "UPDATE work_item_worktrees
                    SET status = 'active'
                  WHERE work_item_id = $1
                    AND worktree_path = $2",
            )
            .bind(item.work_item_id)
            .bind(worktree_path.to_string_lossy().to_string())
            .execute(pg)
            .await?;
        }
        Err(e) => {
            mark_worktree_failed(pg, item.work_item_id, &e.to_string()).await?;
            return Err(e);
        }
    }

    Ok(WorktreeRecord {
        worktree_path,
        base_branch,
        task_branch,
    })
}

async fn insert_worktree_creating(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree_path: &Path,
    base_branch: &str,
    task_branch: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO work_item_worktrees
            (work_item_id, computer_id, sub_agent_id, repo_path, worktree_path,
             base_branch, task_branch, status)
        VALUES ($1, $2, $3, $4, $5, $6, $7, 'creating')
        ON CONFLICT (task_branch) DO UPDATE
            SET computer_id = EXCLUDED.computer_id,
                sub_agent_id = EXCLUDED.sub_agent_id,
                repo_path = EXCLUDED.repo_path,
                worktree_path = EXCLUDED.worktree_path,
                base_branch = EXCLUDED.base_branch,
                status = 'creating',
                cleaned_at = NULL
        "#,
    )
    .bind(item.work_item_id)
    .bind(item.computer_id)
    .bind(item.sub_agent_id)
    .bind(item.repo_path.to_string_lossy().to_string())
    .bind(worktree_path.to_string_lossy().to_string())
    .bind(base_branch)
    .bind(task_branch)
    .execute(pg)
    .await?;
    Ok(())
}

async fn mark_building(pg: &PgPool, item: &AssignedWorkItem) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query("UPDATE work_items SET status = 'building' WHERE id = $1")
        .bind(item.work_item_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE work_item_leases
            SET lease_state = 'building', heartbeat_at = NOW()
          WHERE work_item_id = $1
            AND sub_agent_id = $2
            AND released_at IS NULL",
    )
    .bind(item.work_item_id)
    .bind(item.sub_agent_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_ready_for_review(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
    head_sha: &str,
    pr_url: &str,
) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query(
        "UPDATE work_items
            SET status = 'in_review',
                branch_name = $2,
                pr_url = $3
          WHERE id = $1",
    )
    .bind(item.work_item_id)
    .bind(&worktree.task_branch)
    .bind(pr_url)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE work_item_worktrees
            SET status = 'ready_for_review',
                head_sha = $2
          WHERE work_item_id = $1
            AND task_branch = $3",
    )
    .bind(item.work_item_id)
    .bind(head_sha)
    .bind(&worktree.task_branch)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO work_item_merge_queue
            (work_item_id, project_id, status, branch_name, pr_url, head_sha)
        VALUES ($1, $2, 'queued', $3, $4, $5)
        ON CONFLICT (work_item_id) DO UPDATE
            SET status = 'queued',
                branch_name = EXCLUDED.branch_name,
                pr_url = EXCLUDED.pr_url,
                head_sha = EXCLUDED.head_sha,
                failed_at = NULL,
                failure_reason = NULL
        "#,
    )
    .bind(item.work_item_id)
    .bind(&item.project_id)
    .bind(&worktree.task_branch)
    .bind(pr_url)
    .bind(head_sha)
    .execute(&mut *tx)
    .await?;

    release_slot_and_lease_tx(&mut tx, item, "ready for review").await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_completed_without_pr(pg: &PgPool, item: &AssignedWorkItem) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query(
        "UPDATE work_items
            SET status = 'done',
                completed_at = NOW(),
                last_error = NULL
          WHERE id = $1",
    )
    .bind(item.work_item_id)
    .execute(&mut *tx)
    .await?;
    release_slot_and_lease_tx(&mut tx, item, "no commits produced").await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_failed_and_release(pg: &PgPool, item: &AssignedWorkItem, error: &str) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query(
        "UPDATE work_items
            SET status = 'failed',
                last_error = $2
          WHERE id = $1",
    )
    .bind(item.work_item_id)
    .bind(truncate_for_db(error))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE work_item_worktrees
            SET status = 'failed'
          WHERE work_item_id = $1
            AND status IN ('creating', 'active')",
    )
    .bind(item.work_item_id)
    .execute(&mut *tx)
    .await?;
    release_slot_and_lease_tx(&mut tx, item, "dispatch failed").await?;
    tx.commit().await?;
    Ok(())
}

async fn release_slot_and_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    item: &AssignedWorkItem,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE work_item_leases
            SET lease_state = 'released',
                released_at = NOW(),
                release_reason = $3
          WHERE work_item_id = $1
            AND sub_agent_id = $2
            AND released_at IS NULL",
    )
    .bind(item.work_item_id)
    .bind(item.sub_agent_id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "UPDATE sub_agents
            SET current_work_item_id = NULL,
                status = 'idle',
                started_at = NULL,
                last_heartbeat_at = NOW()
          WHERE id = $1
            AND current_work_item_id = $2",
    )
    .bind(item.sub_agent_id)
    .bind(item.work_item_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn mark_worktree_failed(pg: &PgPool, work_item_id: Uuid, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE work_item_worktrees
            SET status = 'failed'
          WHERE work_item_id = $1
            AND status IN ('creating', 'active')",
    )
    .bind(work_item_id)
    .execute(pg)
    .await?;

    sqlx::query(
        "UPDATE work_items
            SET last_error = $2
          WHERE id = $1",
    )
    .bind(work_item_id)
    .bind(truncate_for_db(error))
    .execute(pg)
    .await?;
    Ok(())
}

async fn mark_worktree_cleaned(pg: &PgPool, work_item_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE work_item_worktrees
            SET status = 'cleaned',
                cleaned_at = NOW()
          WHERE work_item_id = $1
            AND status IN ('active', 'failed')",
    )
    .bind(work_item_id)
    .execute(pg)
    .await?;
    Ok(())
}

/// The prompt the dispatch sends to the agent for a work_item.
fn dispatch_prompt(item: &AssignedWorkItem) -> String {
    match item.description.as_deref() {
        Some(desc) if !desc.trim().is_empty() => format!("{}\n\n{}", item.title, desc.trim()),
        _ => item.title.clone(),
    }
}

/// Parse the token count a vendor CLI reports in its output so the training
/// corpus captures token economics, not just content. codex prints
/// `tokens used\n9,332`; kimi/others print variants like `Tokens: 1234` or
/// `total tokens: 1234`. Best-effort — returns 0 when no count is found.
fn parse_cli_tokens(output: &str) -> i32 {
    let lower = output.to_ascii_lowercase();
    // Find a "tokens" marker, then the nearest number after it (strip commas).
    for marker in ["tokens used", "total tokens", "tokens:", "tokens"] {
        if let Some(pos) = lower.find(marker) {
            let tail = &output[pos + marker.len()..];
            let digits: String = tail
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| *c != ',')
                .collect();
            if let Ok(n) = digits.parse::<i32>() {
                return n;
            }
        }
    }
    0
}

/// Record a dispatch turn in `ff_interactions` (training data). Best-effort —
/// never fails the dispatch. `ff cli` is a thin pass-through that doesn't log,
/// so the dispatch logs its own request/response here.
async fn record_dispatch_interaction(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worker_name: &str,
    backend: &str,
    result: &Result<Output>,
    elapsed: Duration,
) {
    let (response_text, outcome, error_text, tokens_out) = match result {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout)
                .chars()
                .take(16000)
                .collect::<String>();
            let toks = parse_cli_tokens(&text);
            (text, "success".to_string(), None, toks)
        }
        Err(e) => (
            String::new(),
            "error".to_string(),
            // Full anyhow chain ({:#}) so the real cause is captured, not just
            // the top-level wrapper (e.g. "fleet_oneshot round 1").
            Some(format!("{e:#}").chars().take(2000).collect::<String>()),
            0,
        ),
    };
    let rec = ff_db::InteractionRecord {
        channel: "work_item_dispatch".to_string(),
        request_text: dispatch_prompt(item),
        engine: Some(backend.to_string()),
        response_text,
        tokens_out,
        latency_ms: i32::try_from(elapsed.as_millis()).ok(),
        outcome,
        error_text,
        worker_name: Some(worker_name.to_string()),
        endpoint: Some(format!("ff cli {backend}")),
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(pg, &rec).await {
        warn!(error = %e, "work_item_dispatch: failed to log interaction (non-fatal)");
    }
}

/// Edit the worktree to satisfy the work_item. Two lanes, self-healing:
///   Lane 1 (cheap, LOCAL): the `codegen_apply` harness — a local fleet coder
///     emits SEARCH/REPLACE edits, applied + `cargo check`ed + verified-non-empty
///     (region-context handles big files). $0, spreads across the fleet.
///   Lane 2 (BACKSTOP): the prior `ff cli codex --require-change` path, if the
///     local lane can't land it (no-op/malformed/giant file).
/// Returns a synthetic Output on the local-lane win so the caller's commit→PR
/// flow is unchanged. This is what makes the Pillar-4 daemon code unattended on
/// the local fleet instead of always burning the codex lane.
async fn run_ff_dispatch(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
) -> Result<(String, Output)> {
    let prompt = dispatch_prompt(item);

    // Lane 1: local codegen harness.
    match crate::codegen_apply::codegen_apply(pg, &worktree.worktree_path, &prompt, None, 4).await {
        Ok(outcome) if outcome.applied => {
            info!(
                work_item_id = %item.work_item_id,
                rounds = outcome.rounds,
                "work_item_dispatch: local codegen harness landed the change"
            );
            return Ok((
                "local".to_string(),
                synthetic_output(&outcome.final_diff.unwrap_or_else(|| "applied".into())),
            ));
        }
        Ok(outcome) => info!(
            work_item_id = %item.work_item_id,
            error = ?outcome.error,
            "work_item_dispatch: local codegen didn't land; backstop to codex"
        ),
        Err(e) => warn!(
            work_item_id = %item.work_item_id,
            // Full anyhow chain so the REAL cause surfaces (e.g. the underlying
            // fleet_oneshot failure), not just the "fleet_oneshot round 1" wrapper.
            error = format!("{e:#}"),
            "work_item_dispatch: local codegen errored; backstop to codex"
        ),
    }

    // Lane 2: dispatch to an AVAILABLE backend (capability A4/A5) with the full
    // cloud-error nervous system wired in. The router returns this node's
    // dispatchable backends headroom/rank-ordered. For each, we run `ff cli
    // <backend>`; on a cloud failure we CLASSIFY the CLI output
    // (`cloud_error::classify`), record it to the provider circuit-breaker, then:
    //   • transient (529/429/5xx/timeout/network) → back off + AUTO-CONTINUE in
    //     place (council: ≤2 re-injections, 5s then 20s) — the headless "continue"
    //     a human would otherwise have to type;
    //   • breaker-tripped / terminal-for-this-backend (auth/quota/overload-after-
    //     retries) → SWITCH to the next backend.
    // So a 529 on codex retries then fails over to claude unattended, instead of
    // dying. Falls back to `codex` only when no backend is known dispatchable.
    let routed = ff_db::pg_routed_backends(pg, item.computer_id, 5400)
        .await
        .unwrap_or_default();
    let backends = if routed.is_empty() {
        vec!["codex".to_string()]
    } else {
        routed
    };
    let computer_id = item.computer_id;
    let mut last_output: Option<(String, Output)> = None;

    for backend in &backends {
        if crate::circuit_breaker::is_provider_open(pg, computer_id, backend)
            .await
            .unwrap_or(false)
        {
            info!(backend = %backend, "run_ff_dispatch: skipping breaker-open backend");
            continue;
        }
        let mut attempt: u32 = 0;
        loop {
            let out = match run_backend_cli(backend, &worktree.worktree_path, &prompt).await {
                Ok(o) => o,
                Err(e) => {
                    // A timeout / spawn error is a `Timeout`-class provider fault.
                    // BUT the CLI (esp. codex) often writes a complete, valid diff
                    // early and then just fails to EXIT — so a timeout doesn't mean
                    // "no work done". If the worktree already has a real diff,
                    // SALVAGE it (treat as success → commit → PR) instead of
                    // discarding it and failing over. CI verifies the diff anyway.
                    // (dogfooded 2026-07-01: `ff usage` wrote a full 2-file change
                    // then timed out, and the work was thrown away.)
                    if worktree_has_diff(&worktree.worktree_path) {
                        warn!(backend = %backend, error = %e, "run_ff_dispatch: backend timed out but wrote a real diff — salvaging");
                        let _ = crate::circuit_breaker::record_provider_success(
                            pg,
                            computer_id,
                            backend,
                        )
                        .await;
                        return Ok((
                            backend.clone(),
                            synthetic_output("salvaged diff after backend timeout"),
                        ));
                    }
                    // No diff → genuine failure: record it and SWITCH to the next
                    // backend rather than `?`-propagating out (which would abort
                    // failover — the "codex hangs → whole dispatch dies" bug).
                    warn!(backend = %backend, error = %e, "run_ff_dispatch: backend run errored (timeout/spawn), no diff — switching");
                    let _ = crate::circuit_breaker::record_provider_failure(
                        pg,
                        computer_id,
                        backend,
                        "timeout",
                    )
                    .await;
                    break; // try the next routed backend
                }
            };
            if out.status.success() {
                let _ =
                    crate::circuit_breaker::record_provider_success(pg, computer_id, backend).await;
                // Clean run → full headroom signal (self-corrects a prior limit).
                let _ =
                    crate::circuit_breaker::record_usage_signal(pg, computer_id, backend, 100.0)
                        .await;
                if attempt > 0 || backend != &backends[0] {
                    info!(backend = %backend, attempt, "run_ff_dispatch: recovered via auto-continue/failover");
                }
                return Ok((backend.clone(), out));
            }
            // A `--require-change` no-op (exit 3) is a task-level failure, not a
            // provider fault — surface it without classify/retry/switch.
            if out.status.code() == Some(3) {
                return Ok((backend.clone(), out));
            }
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let class = crate::cloud_error::classify(backend, out.status.code(), &combined);
            let tripped = crate::circuit_breaker::record_provider_failure(
                pg,
                computer_id,
                backend,
                class.as_str(),
            )
            .await
            .unwrap_or(false);
            warn!(
                backend = %backend, class = class.as_str(), attempt, breaker_tripped = tripped,
                "run_ff_dispatch: backend error classified"
            );
            // Capture a usage-headroom signal from limit/quota/overload errors so
            // the router deprioritizes this provider until it recovers.
            if let Some(rem) = crate::circuit_breaker::headroom_hint_for_category(class.as_str()) {
                let _ = crate::circuit_breaker::record_usage_signal(pg, computer_id, backend, rem)
                    .await;
            }
            last_output = Some((backend.clone(), out));
            if class.is_transient() && attempt < AUTO_CONTINUE_MAX && !tripped {
                let backoff = if attempt == 0 { 5 } else { 20 };
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                attempt += 1;
                continue; // auto-continue on the same backend
            }
            break; // exhausted / terminal-for-this-backend → try the next one
        }
    }
    last_output
        .map(Ok)
        .unwrap_or_else(|| bail!("run_ff_dispatch: no dispatchable backend on this node"))
}

/// The backend name for interaction attribution when a dispatch errored before
/// any backend produced output (so `run_ff_dispatch` returned Err, carrying no
/// backend). Best-effort: the first routed backend, else the historical default.
async fn primary_dispatch_backend(pg: &PgPool, computer_id: Uuid) -> String {
    ff_db::pg_routed_backends(pg, computer_id, 5400)
        .await
        .ok()
        .and_then(|b| b.into_iter().next())
        .unwrap_or_else(|| "codex".to_string())
}

/// Council cap: how many headless auto-continue re-injections to attempt on a
/// transient cloud error before switching providers.
const AUTO_CONTINUE_MAX: u32 = 2;

/// Run `ff cli <backend>` against the worktree once and capture its Output.
async fn run_backend_cli(backend: &str, cwd: &Path, prompt: &str) -> Result<Output> {
    let backend = backend.to_string();
    let cwd = cwd.to_path_buf();
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("ff");
        cmd.arg("cli")
            .arg(&backend)
            .arg("--cwd")
            .arg(&cwd)
            .arg("--timeout")
            .arg(FF_TIMEOUT_SECS.to_string())
            // Fail (exit 3) if the CLI exits 0 but changes nothing — a no-op run is
            // a failed work_item, not a silent 'done' (catches stdin-pipe no-ops).
            .arg("--require-change")
            .arg(&prompt);
        run_command_timeout(cmd, Duration::from_secs(FF_TIMEOUT_SECS + 30))
    })
    .await
    .context("join ff dispatch task")?
}

/// True if the worktree has any uncommitted change (tracked edits or new files).
/// Used to SALVAGE a backend that wrote a valid diff but timed out before
/// exiting — the work is real even though the process didn't terminate cleanly.
fn worktree_has_diff(worktree_path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Build a success `Output` for the local-codegen lane so `dispatch_one`'s
/// existing interaction-logging + commit→PR flow needs no special-casing.
fn synthetic_output(summary: &str) -> Output {
    use std::os::unix::process::ExitStatusExt;
    Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: summary.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn git_worktree_add(
    repo_path: &Path,
    worktree_path: &Path,
    base_branch: &str,
    task_branch: &str,
) -> Result<()> {
    let base_ref = format!("origin/{base_branch}");
    let _ = run_git(
        repo_path,
        ["fetch", "origin", base_branch],
        Duration::from_secs(120),
    );

    run_git(
        repo_path,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-B"),
            OsStr::new(task_branch),
            worktree_path.as_os_str(),
            OsStr::new(&base_ref),
        ],
        Duration::from_secs(120),
    )
    .or_else(|_| {
        run_git(
            repo_path,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-B"),
                OsStr::new(task_branch),
                worktree_path.as_os_str(),
                OsStr::new(base_branch),
            ],
            Duration::from_secs(120),
        )
    })?;
    Ok(())
}

fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    if worktree_path.exists() {
        run_git(
            repo_path,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                worktree_path.as_os_str(),
            ],
            Duration::from_secs(120),
        )?;
    }
    let _ = run_git(repo_path, ["worktree", "prune"], Duration::from_secs(60));
    Ok(())
}

fn reclaim_build_artifacts(path: &Path) -> u64 {
    fn is_reclaimable_dir_name(name: &OsStr) -> bool {
        name == OsStr::new("target")
            || name == OsStr::new("node_modules")
            || name == OsStr::new(".venv")
    }

    fn approximate_size(path: &Path) -> u64 {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        if metadata.file_type().is_symlink() {
            return 0;
        }
        if metadata.is_file() {
            return metadata.len();
        }
        if !metadata.is_dir() {
            return 0;
        }

        let mut total = metadata.len();
        let Ok(entries) = std::fs::read_dir(path) else {
            return total;
        };
        for entry in entries.flatten() {
            total = total.saturating_add(approximate_size(&entry.path()));
        }
        total
    }

    fn walk(path: &Path) -> u64 {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return 0;
        }

        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };

        let mut reclaimed = 0u64;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }

            if is_reclaimable_dir_name(&entry.file_name()) {
                let bytes = approximate_size(&entry_path);
                if std::fs::remove_dir_all(&entry_path).is_ok() {
                    reclaimed = reclaimed.saturating_add(bytes);
                }
            } else {
                reclaimed = reclaimed.saturating_add(walk(&entry_path));
            }
        }
        reclaimed
    }

    walk(path)
}

/// Stage + commit any agent-made changes in the worktree. Returns true if a
/// commit was created, false if the worktree was clean (agent made no change).
/// Provides a deterministic author so the daemon's git env needn't be configured.
fn commit_worktree_changes(worktree_path: &Path, title: &str) -> Result<bool> {
    run_git(worktree_path, ["add", "-A"], Duration::from_secs(60))?;
    let status = run_git(
        worktree_path,
        ["status", "--porcelain"],
        Duration::from_secs(30),
    )?;
    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Ok(false); // nothing to commit
    }
    let msg = format!(
        "{}\n\nAutomated work_item dispatch (ForgeFleet Pillar 4).",
        title
    );
    run_git(
        worktree_path,
        [
            OsStr::new("-c"),
            OsStr::new("user.name=ForgeFleet"),
            OsStr::new("-c"),
            OsStr::new("user.email=fleet@forgefleet.local"),
            OsStr::new("commit"),
            OsStr::new("-m"),
            OsStr::new(&msg),
        ],
        Duration::from_secs(60),
    )?;
    Ok(true)
}

fn branch_has_commits(repo_path: &Path, base_branch: &str, task_branch: &str) -> Result<bool> {
    let range = format!("origin/{base_branch}..{task_branch}");
    let out = run_git(
        repo_path,
        [
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new(&range),
        ],
        Duration::from_secs(30),
    )
    .or_else(|_| {
        let range = format!("{base_branch}..{task_branch}");
        run_git(
            repo_path,
            [
                OsStr::new("rev-list"),
                OsStr::new("--count"),
                OsStr::new(&range),
            ],
            Duration::from_secs(30),
        )
    })?;
    let count = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    Ok(count > 0)
}

fn git_head_sha(worktree_path: &Path) -> Result<String> {
    let out = run_git(
        worktree_path,
        ["rev-parse", "HEAD"],
        Duration::from_secs(30),
    )?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn push_branch(repo_path: &Path, task_branch: &str) -> Result<()> {
    run_git(
        repo_path,
        [
            OsStr::new("push"),
            OsStr::new("-u"),
            OsStr::new("origin"),
            OsStr::new(task_branch),
        ],
        Duration::from_secs(300),
    )?;
    Ok(())
}

async fn create_pr(
    worktree_path: &Path,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
) -> Result<String> {
    let body = format!(
        "Automated work_item dispatch.\n\nwork_item_id: {}\nbranch: {}",
        item.work_item_id, worktree.task_branch
    );

    let mut cmd = Command::new("gh");
    cmd.current_dir(worktree_path)
        .arg("pr")
        .arg("create")
        .arg("--title")
        .arg(&item.title)
        .arg("--body")
        .arg(body)
        .arg("--head")
        .arg(&worktree.task_branch)
        .arg("--base")
        .arg(&worktree.base_branch);
    if let Some(token) = crate::fleet_info::fetch_secret("github_gh_token").await {
        cmd.env("GH_TOKEN", token);
    } else {
        warn!(
            work_item_id = %item.work_item_id,
            "work_item_dispatch: github_gh_token secret missing; falling back to ambient gh auth"
        );
    }
    let out = run_command_timeout(cmd, Duration::from_secs(120))?;
    let pr_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if pr_url.is_empty() {
        bail!("gh pr create returned an empty PR URL");
    }
    Ok(pr_url)
}

fn default_branch(repo_path: &Path) -> Result<String> {
    let out = run_git(
        repo_path,
        ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        Duration::from_secs(30),
    )?;
    let branch = String::from_utf8_lossy(&out.stdout)
        .trim()
        .strip_prefix("origin/")
        .unwrap_or(String::from_utf8_lossy(&out.stdout).trim())
        .to_string();
    if branch.is_empty() {
        bail!("origin/HEAD did not resolve to a branch");
    }
    Ok(branch)
}

fn run_git<I, S>(cwd: &Path, args: I, timeout: Duration) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    for arg in args {
        cmd.arg(arg);
    }
    run_command_timeout(cmd, timeout)
}

fn run_command_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let program = format!("{cmd:?}");
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    let start = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            let output = child
                .wait_with_output()
                .with_context(|| format!("collect output for {program}"))?;
            if output.status.success() {
                return Ok(output);
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "command failed: {program}\nstatus: {}\nstdout: {}\nstderr: {}",
                output.status,
                truncate_for_log(&stdout),
                truncate_for_log(&stderr)
            );
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command timed out after {:?}: {program}", timeout);
        }

        std::thread::sleep(Duration::from_millis(COMMAND_POLL_MS));
    }
}

fn truncate_for_db(s: &str) -> String {
    const MAX: usize = 4000;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 2000;
    let trimmed = s.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

// ── Pillar 4 worktree reaper (per-host) ──────────────────────────────────
// Removes on-disk git worktrees whose work_item reached a terminal state
// (cancelled/merged/failed/done) but whose worktree row isn't 'cleaned' yet.
// Host-agnostic by design: each host reaps only its OWN worktrees (a remote
// worktree can't be removed from another host, which is why `ff pm cancel`
// can't do it — this tick can). Never touches 'in_review' items (PR open).

/// One reaper pass. Returns the number of worktrees cleaned.
pub async fn evaluate_worktree_reaper(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let reapable = ff_db::pg_reapable_worktrees(pg, worker_name).await?;
    let mut reaped = 0usize;
    let mut reclaimed_bytes = 0u64;
    for wt in reapable {
        let repo = PathBuf::from(&wt.repo_path);
        let tree = PathBuf::from(&wt.worktree_path);
        // Best-effort filesystem cleanup; the DB mark below is the source of truth.
        let _ = remove_worktree(&repo, &tree);
        reclaimed_bytes = reclaimed_bytes.saturating_add(reclaim_build_artifacts(&tree));
        let _ = run_git(
            &repo,
            [
                OsStr::new("branch"),
                OsStr::new("-D"),
                OsStr::new(&wt.task_branch),
            ],
            Duration::from_secs(30),
        );
        sqlx::query(
            "UPDATE work_item_worktrees SET status = 'cleaned', cleaned_at = NOW() \
              WHERE work_item_id = $1",
        )
        .bind(wt.work_item_id)
        .execute(pg)
        .await?;
        reaped += 1;
    }
    if reaped > 0 {
        info!(
            reaped,
            reclaimed_bytes, "worktree_reaper: cleaned terminal worktrees"
        );
    }
    Ok(reaped)
}

/// Spawn the per-host worktree-reaper loop (not leader-gated — each host cleans
/// its own worktrees).
pub fn spawn_worktree_reaper(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = evaluate_worktree_reaper(&pg, &worker_name).await {
                        warn!(error = %e, "worktree_reaper tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("worktree_reaper loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::parse_cli_tokens;

    #[test]
    fn parses_codex_tokens_used_with_comma() {
        // The exact shape codex prints.
        assert_eq!(parse_cli_tokens("codex\nHELLO\ntokens used\n9,332\n"), 9332);
    }

    #[test]
    fn parses_inline_tokens_variants() {
        assert_eq!(parse_cli_tokens("Total tokens: 1234"), 1234);
        assert_eq!(parse_cli_tokens("done. tokens: 42"), 42);
    }

    #[test]
    fn returns_zero_when_absent() {
        assert_eq!(parse_cli_tokens("no counts here, just output"), 0);
        assert_eq!(parse_cli_tokens(""), 0);
    }
}
