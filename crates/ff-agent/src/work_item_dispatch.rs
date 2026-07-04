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
/// Ceiling on how many work_items a single host starts in one dispatch tick.
/// The effective budget per tick is [`dispatch_budget_for_host`], which scales
/// with the host's free sub-agent slots up to this cap (and drops to 1 under
/// backpressure). Replaces the old hard `1/tick`, which left the fleet mostly
/// idle even with many ready tasks and dozens of free slots.
const MAX_DISPATCH_PER_TICK: i64 = 3;

/// Recent-failure count at/above which the host throttles back to a single
/// dispatch per tick (backpressure — stop feeding a host that's failing).
const BACKPRESSURE_FAILURE_THRESHOLD: usize = 3;

/// Capacity-aware per-tick dispatch budget for one host: dispatch up to as many
/// items as it has free slots, capped at [`MAX_DISPATCH_PER_TICK`]. If the host
/// has recently failed a lot (`recent_failures >= BACKPRESSURE_FAILURE_THRESHOLD`)
/// throttle back to 1 so a broken host/lane doesn't burn a batch of tasks. Pure
/// so it's unit-testable. Always returns at least 1 when there is ≥1 free slot,
/// and 0 when there are no free slots.
fn dispatch_budget_for_host(free_slots: usize, recent_failures: usize) -> i64 {
    if free_slots == 0 {
        return 0;
    }
    if recent_failures >= BACKPRESSURE_FAILURE_THRESHOLD {
        return 1;
    }
    let cap = MAX_DISPATCH_PER_TICK.max(1) as usize;
    free_slots.min(cap) as i64
}

#[derive(Debug, Clone)]
struct AssignedWorkItem {
    work_item_id: Uuid,
    project_id: String,
    title: String,
    description: Option<String>,
    base_branch: Option<String>,
    repo_id: Option<Uuid>,
    repo_url: Option<String>,
    repo_path: PathBuf,
    sub_agent_id: Uuid,
    computer_id: Uuid,
    slot: i32,
    /// Prior failed attempts (escalation ladder). Drives backend escalation
    /// (local → cloud) and prompt context injection.
    attempts: i32,
    /// The error from the previous attempt, fed back into this attempt's prompt
    /// so the model doesn't repeat the same mistake (retry-with-context).
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct WorktreeRecord {
    worktree_path: PathBuf,
    base_branch: String,
    task_branch: String,
}

/// Count this host's recently-failed work_item dispatches (last 15 min), used
/// as the backpressure signal for [`dispatch_budget_for_host`]. Best-effort —
/// the caller treats an error as "0 failures" (no backpressure).
async fn recent_host_failures(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let row: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM work_item_leases l \
           JOIN computers c ON c.id = l.computer_id \
           JOIN work_items w ON w.id = l.work_item_id \
          WHERE c.name = $1 \
            AND w.status = 'failed' \
            AND l.created_at > NOW() - INTERVAL '15 minutes'",
    )
    .bind(worker_name)
    .fetch_one(pg)
    .await?;
    Ok(row.0.max(0) as usize)
}

/// One dispatch pass. Returns the number of work_items started by this host.
pub async fn evaluate_work_item_dispatch(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let repo_path = std::env::current_dir().context("resolve current repo path")?;

    // Capacity-aware budget: dispatch up to this host's free-slot count, capped
    // at MAX_DISPATCH_PER_TICK, throttled to 1 under recent-failure backpressure.
    // Replaces the old hard 1/tick that left the fleet mostly idle.
    let free_slots = ff_db::pg_free_slots(pg, Some(worker_name), MAX_DISPATCH_PER_TICK)
        .await
        .map(|s| s.len())
        .unwrap_or(1);
    let recent_failures = recent_host_failures(pg, worker_name).await.unwrap_or(0);
    let budget = dispatch_budget_for_host(free_slots, recent_failures).max(1);

    let assigned = assigned_work_items(pg, worker_name, &repo_path, budget).await?;
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
                if let Err(cleanup) = requeue_or_fail(pg, &item, &e.to_string()).await {
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

fn default_clone_path(project_id: &str, repo_url: &str) -> PathBuf {
    let slug_source = repo_url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(repo_url);
    let slug: String = slug_source
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("project-repos")
        .join(project_id)
        .join(if slug.is_empty() { "repo" } else { slug })
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
            w.repo_id,
            COALESCE(w.attempts, 0) AS attempts,
            w.last_error,
            COALESCE(NULLIF(w.repo_url, ''), NULLIF(wr.github_url, '')) AS repo_url,
            NULLIF(w.repo_path, '') AS bound_repo_path,
            NULLIF(w.metadata->>'repo_path', '') AS metadata_repo_path,
            -- Legacy/default path resolution (per-project, V141 project_folders):
            -- explicit work_items.repo_path wins in Rust below; else historical
            -- metadata override; else this project's local folder on THIS host
            -- (host-specific row preferred, then canonical computer_id-NULL);
            -- else host source_tree_path; else daemon cwd.
            COALESCE(
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
            ) AS fallback_repo_path,
            sa.id AS sub_agent_id,
            sa.computer_id,
            sa.slot
          FROM sub_agents sa
          JOIN computers c ON c.id = sa.computer_id
          JOIN work_items w ON w.id = sa.current_work_item_id
          LEFT JOIN project_repos wr ON wr.id = w.repo_id
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
            let repo_url: Option<String> = r.try_get("repo_url").ok().flatten();
            let bound_repo_path: Option<PathBuf> = r
                .try_get::<Option<String>, _>("bound_repo_path")
                .ok()
                .flatten()
                .map(|p| PathBuf::from(expand_tilde(&p)));
            let metadata_repo_path: Option<PathBuf> = r
                .try_get::<Option<String>, _>("metadata_repo_path")
                .ok()
                .flatten()
                .map(|p| PathBuf::from(expand_tilde(&p)));
            let fallback_repo_path: String = r.get("fallback_repo_path");
            let local_bound_path = bound_repo_path.as_ref().filter(|p| p.exists()).cloned();
            let repo_path = local_bound_path
                .or(metadata_repo_path)
                .or_else(|| {
                    repo_url
                        .as_deref()
                        .map(|url| default_clone_path(&r.get::<String, _>("project_id"), url))
                })
                .or(bound_repo_path)
                .unwrap_or_else(|| PathBuf::from(fallback_repo_path));
            Ok(AssignedWorkItem {
                work_item_id: r.get("work_item_id"),
                project_id: r.get("project_id"),
                title: r.get("title"),
                description: r.try_get("description").ok().flatten(),
                base_branch: r.try_get("base_branch").ok().flatten(),
                repo_id: r.try_get("repo_id").ok().flatten(),
                repo_url,
                repo_path,
                sub_agent_id: r.get("sub_agent_id"),
                computer_id: r.get("computer_id"),
                slot: r.get("slot"),
                attempts: r.try_get("attempts").unwrap_or(0),
                last_error: r.try_get("last_error").ok().flatten(),
            })
        })
        .collect()
}

async fn ensure_repo_checked_out(pg: &PgPool, item: &AssignedWorkItem) -> Result<()> {
    if item.repo_path.exists() && item.repo_path.join(".git").exists() {
        return Ok(());
    }

    let github_url = if item
        .repo_url
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        item.repo_url.clone()
    } else if let Some(repo_id) = item.repo_id {
        sqlx::query_scalar(
            "SELECT github_url
               FROM project_repos
              WHERE id = $1
                AND NULLIF(github_url, '') IS NOT NULL
              LIMIT 1",
        )
        .bind(repo_id)
        .fetch_optional(pg)
        .await
        .with_context(|| format!("lookup repo {repo_id} for work_item {}", item.work_item_id))?
    } else {
        sqlx::query_scalar(
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
        .with_context(|| format!("lookup primary repo for project {}", item.project_id))?
    };

    let github_url = github_url.ok_or_else(|| {
        anyhow!(
            "repo path {} is not a git repo and work_item {} has no repo_url/project repo to clone",
            item.repo_path.display(),
            item.work_item_id
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

    // Keep the lease heartbeat alive for the ENTIRE dispatch — the backend build
    // AND the commit/push/PR tail — via an RAII guard that stops it when
    // dispatch_one returns on ANY path. Previously the heartbeat stopped the
    // instant the backend CLI returned, so a slow `git push` / `gh pr create` on
    // a big diff ran with a frozen lease and the watchdog could reap it
    // mid-finalize as a "stale-heartbeat takeover".
    let _heartbeat_guard = HeartbeatGuard::spawn(item.work_item_id);

    let started = std::time::Instant::now();
    let dispatch_full = run_ff_dispatch(&pg, &item, &worktree).await;

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

/// RAII guard that keeps a work_item lease's heartbeat fresh while alive and
/// signals the heartbeat task to stop on drop — i.e. when `dispatch_one` returns
/// on ANY path (success, no-commit early return, or error). This holds the lease
/// for the whole dispatch (build → commit → push → PR) so the leader watchdog
/// can't reap it mid-finalize.
struct HeartbeatGuard {
    stop_tx: watch::Sender<bool>,
}

impl HeartbeatGuard {
    fn spawn(work_item_id: Uuid) -> Self {
        let (stop_tx, stop_rx) = watch::channel(false);
        // Detached: the task loops on its own timer and exits promptly when the
        // guard's drop sends `true` on stop_tx.
        let _ = spawn_heartbeat(work_item_id, stop_rx);
        Self { stop_tx }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
}

fn spawn_heartbeat(
    work_item_id: Uuid,
    mut stop_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    heartbeat_lease_once(work_item_id).await;
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

/// Refresh a lease heartbeat via the DEDICATED heartbeat pool, with bounded
/// retry + LOUD logging. The old path shared the node's main pool and swallowed
/// the error, so under concurrent dispatch a transient acquire-timeout silently
/// skipped the beat — `heartbeat_at` went stale and the watchdog reaped a live
/// build ("stale-heartbeat takeover"). The dedicated pool isolates the beat from
/// dispatch/tick contention; the retry rides out a genuine DB hiccup within the
/// beat interval; a final failure is logged loudly instead of vanishing.
/// (ff council codex+kimi 2026-07-04.)
async fn heartbeat_lease_once(work_item_id: Uuid) {
    for attempt in 0..3u32 {
        match crate::fleet_info::get_heartbeat_pool().await {
            Ok(pool) => match ff_db::pg_heartbeat_work_item_lease(&pool, work_item_id).await {
                Ok(()) => return,
                Err(e) => warn!(
                    work_item_id = %work_item_id, attempt, error = %e,
                    "work_item_dispatch: lease heartbeat UPDATE failed; retrying on dedicated pool"
                ),
            },
            Err(e) => warn!(
                work_item_id = %work_item_id, attempt, error = %e,
                "work_item_dispatch: heartbeat pool unavailable; retrying"
            ),
        }
        tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt + 1))).await;
    }
    warn!(
        work_item_id = %work_item_id,
        "work_item_dispatch: lease heartbeat FAILED after 3 tries — lease may go stale (watchdog could reap a LIVE build)"
    );
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

/// Max dispatch attempts before a work_item is escalated to terminal `failed`
/// instead of being requeued. Each retry re-runs the dispatch with the prior
/// error appended to the item's `last_error` so the next attempt has context.
const MAX_DISPATCH_ATTEMPTS: i32 = 3;

/// Escalation ladder stage 2: after this many prior attempts, SKIP the local
/// codegen lane (it has already failed the same way) and go straight to the
/// cloud/CLI backstop, which is more capable. Below this, try cheap-local-first.
const ESCALATE_TO_CLOUD_AT: i32 = 2;

/// Hard ceiling on the Lane-1 LOCAL codegen harness. It can HANG on a complex
/// prompt (observed 2026-07-04 on the fleet_self_heal fold: empty worktree, no
/// progress, live heartbeat for 30+ min — the dispatch's OUTER heartbeat loop
/// keeps the lease fresh, so the lease reaper never reclaims it and the slot
/// stalls forever). A hung local lane must FAIL OVER to the cloud backstop, not
/// wedge the slot. 7 min is well above a real local codegen pass, but bounded.
const LANE1_TIMEOUT_SECS: u64 = 420;

/// Deterministic execution-contract outcome of a dispatch (roadmap item #3).
/// Formalizes what previously was ad-hoc: a dispatch either succeeds, fails with
/// no diff (retryable), fails but left a real diff (salvageable — treat as a
/// success-with-work), or timed out but its diff was salvaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// The backend ran to a clean, successful exit.
    Success,
    /// The backend errored (or was killed) and left NO usable diff — retryable.
    FailedNoDiff,
    /// The backend errored but the worktree has a real diff — salvageable work.
    FailedWithDiff,
    /// The backend timed out and its diff was salvaged into a commit.
    TimeoutSalvaged,
}

/// Classify a dispatch result into the [`DispatchOutcome`] contract. Pure +
/// unit-testable. `worktree_has_diff` is the caller's git-status check on the
/// worktree after the run. A timeout/kill error that nonetheless left a diff is
/// `TimeoutSalvaged`; any other error with a diff is `FailedWithDiff`; an error
/// with no diff is `FailedNoDiff` (the only retryable class); `Ok` is `Success`.
pub fn classify_dispatch_outcome(
    result: &Result<Output>,
    worktree_has_diff: bool,
) -> DispatchOutcome {
    match result {
        Ok(_) => DispatchOutcome::Success,
        Err(e) => {
            if worktree_has_diff {
                let msg = e.to_string().to_ascii_lowercase();
                if msg.contains("timed out") || msg.contains("timeout") {
                    DispatchOutcome::TimeoutSalvaged
                } else {
                    DispatchOutcome::FailedWithDiff
                }
            } else {
                DispatchOutcome::FailedNoDiff
            }
        }
    }
}

/// Failure-aware retry (roadmap item #2). On a dispatch failure, requeue the
/// work_item (status → `ready`, `attempts` + 1, prior error appended to
/// `last_error` so the next run has the failure context) UNTIL `attempts`
/// reaches [`MAX_DISPATCH_ATTEMPTS`], then escalate to terminal `failed` via
/// [`mark_failed_and_release`]. Always releases the slot/lease so the scheduler
/// can re-dispatch. Best-effort: on any DB error, falls back to marking failed
/// so a stuck item can't hold a slot forever.
async fn requeue_or_fail(pg: &PgPool, item: &AssignedWorkItem, error: &str) -> Result<()> {
    let attempts: i32 =
        sqlx::query_scalar("SELECT COALESCE(attempts, 0) FROM work_items WHERE id = $1")
            .bind(item.work_item_id)
            .fetch_optional(pg)
            .await
            .ok()
            .flatten()
            .unwrap_or(0);

    if attempts + 1 >= MAX_DISPATCH_ATTEMPTS {
        info!(
            work_item_id = %item.work_item_id,
            attempts = attempts + 1,
            max = MAX_DISPATCH_ATTEMPTS,
            "work_item_dispatch: retry budget exhausted — escalating to failed"
        );
        return mark_failed_and_release(pg, item, error).await;
    }

    let mut tx = pg.begin().await?;
    // Requeue: back to 'ready', bump attempts, and append the error to last_error
    // so the next attempt's prompt/context can see why the prior run failed.
    sqlx::query(
        "UPDATE work_items
            SET status = 'ready',
                attempts = COALESCE(attempts, 0) + 1,
                last_error = $2
          WHERE id = $1",
    )
    .bind(item.work_item_id)
    .bind(truncate_for_db(&format!(
        "[attempt {}] {}",
        attempts + 1,
        error
    )))
    .execute(&mut *tx)
    .await?;
    // Clear the failed worktree row so a fresh one is created next attempt.
    sqlx::query(
        "UPDATE work_item_worktrees
            SET status = 'failed'
          WHERE work_item_id = $1
            AND status IN ('creating', 'active')",
    )
    .bind(item.work_item_id)
    .execute(&mut *tx)
    .await?;
    release_slot_and_lease_tx(&mut tx, item, "dispatch failed — requeued for retry").await?;
    tx.commit().await?;
    info!(
        work_item_id = %item.work_item_id,
        attempts = attempts + 1,
        "work_item_dispatch: requeued for retry with error context"
    );
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
    // Escalation ladder stage 3: the fleet (local + cloud) couldn't build this
    // after the full retry budget — tell the operator. Best-effort; a notify
    // failure never fails the dispatch.
    notify_operator_task_failed(pg, item.work_item_id, &item.title, error).await;
    Ok(())
}

/// Best-effort Telegram notification when a work_item exhausts its retry budget
/// and lands on terminal `failed` — "Jarvis tells you when it's genuinely
/// stuck." Reads the same `openclaw.telegram_*` secrets the alert evaluator
/// uses. NEVER returns an error or panics: any failure is logged and swallowed.
async fn notify_operator_task_failed(pg: &PgPool, work_item_id: Uuid, title: &str, error: &str) {
    let token = match ff_db::pg_get_secret(pg, "openclaw.telegram_bot_token").await {
        Ok(Some(t)) if !t.trim().is_empty() => t,
        _ => {
            tracing::debug!("notify_operator_task_failed: no telegram bot token; skipping");
            return;
        }
    };
    let chat_id = match ff_db::pg_get_secret(pg, "openclaw.telegram_chat_id").await {
        Ok(Some(c)) if !c.trim().is_empty() => c,
        _ => {
            tracing::debug!("notify_operator_task_failed: no telegram chat id; skipping");
            return;
        }
    };
    // Throttle: collapse a burst of same-signature failures into ONE alert/hour so
    // an incident (e.g. the 2026-07-04 restart loop, dozens of identical "no
    // dispatchable backend" failures) doesn't flood the operator. The dedup key is
    // the error class (first line). Cross-node via the DB (failures fire on
    // whichever node built). FAIL-OPEN: any dedup error → send anyway.
    let signature: String = error
        .lines()
        .next()
        .unwrap_or(error)
        .trim()
        .chars()
        .take(200)
        .collect();
    match sqlx::query_scalar::<_, String>(
        "INSERT INTO operator_notify_dedup (signature, last_sent) VALUES ($1, NOW()) \
         ON CONFLICT (signature) DO UPDATE SET last_sent = NOW() \
           WHERE operator_notify_dedup.last_sent < NOW() - INTERVAL '1 hour' \
         RETURNING signature",
    )
    .bind(&signature)
    .fetch_optional(pg)
    .await
    {
        Ok(None) => {
            tracing::info!(%work_item_id, signature = %signature, "notify_operator_task_failed: throttled (same-signature alert already sent within the hour)");
            return;
        }
        Ok(Some(_)) => {} // first in the window (or window expired) — send
        Err(e) => {
            tracing::warn!(error = %e, "notify_operator_task_failed: dedup check failed; sending anyway (fail-open)")
        }
    }
    let err_clip: String = error.chars().take(800).collect();
    let text = format!(
        "🛑 ForgeFleet task FAILED after max retries\n\n{title}\n\nwork_item: {work_item_id}\n\nlast error:\n{err_clip}"
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "notify_operator_task_failed: client build failed");
            return;
        }
    };
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    match client
        .post(&url)
        .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(%work_item_id, "notify_operator_task_failed: telegram sent");
        }
        Ok(resp) => tracing::warn!(
            status = %resp.status(),
            %work_item_id,
            "notify_operator_task_failed: telegram non-2xx"
        ),
        Err(e) => {
            tracing::warn!(error = %e, %work_item_id, "notify_operator_task_failed: telegram send failed")
        }
    }
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
/// Repo-wide build conventions injected into EVERY dispatch prompt so a sub-agent
/// doesn't rediscover them per task. Distilled from recurring fleet-build failures
/// (2026-07-04): DB tests panicking in CI's DB-less job, redundant second
/// migrations + endless iteration, and edits to historical migration consts.
const DISPATCH_HOUSE_RULES: &str = "\n\n--- ForgeFleet build rules (apply to EVERY change) ---\n\
- DB TESTS: any test that needs Postgres MUST early-return when neither \
FORGEFLEET_POSTGRES_URL nor FORGEFLEET_DATABASE_URL is set (CI's `cargo test --lib` \
has NO database and will PANIC otherwise). Never let a DB test panic in CI.\n\
- MIGRATIONS are forward-only: add ONE new const + register the next integer version; \
NEVER edit an existing migration const, and never add a second/redundant migration.\n\
- STOP when done: once `cargo +1.88.0 fmt --check` + `cargo +1.88.0 check` + your \
targeted test pass, open ONE PR and STOP — do not keep iterating or push more commits.\n\
- Keep the diff minimal and scoped strictly to the task.\n";

/// Whether a stored `last_error` is a TASK-level failure the coding agent can
/// act on (compile error, test failure, lint, missing file, type/assert error)
/// versus an INFRASTRUCTURE failure (backend spawn, heartbeat/lease lifecycle,
/// DB pool, provider/network, host-resource exhaustion) it cannot fix.
///
/// Only actionable errors belong in the retry prompt; injecting infra errors
/// with "diagnose the root cause" makes the agent waste the attempt trying to
/// fix e.g. "no dispatchable backend on this node". Signatures are consolidated
/// from live errors seen in dispatch + an `ff council` (codex+kimi) pass; kept
/// deliberately unambiguous so a real Rust compile/test error is never matched.
fn retry_error_is_actionable(err: &str) -> bool {
    const INFRA_ERROR_SIGNATURES: &[&str] = &[
        // dispatch / backend spawn + routing
        "no dispatchable backend",
        // the surfaced-error bail — suppress it too, else it's re-injected into
        // the retry prompt AND recursively accumulates the prior attempt's
        // context, exploding the prompt with nested "diagnose it" garbage.
        "all backends failed on this node",
        "spawn \"",
        "command timed out",
        "timed out after",
        // heartbeat / lease lifecycle
        "stale-heartbeat",
        "heartbeat takeover",
        // datastore / pool
        "pool timed out",
        "pool timeout",
        "route deployments",
        // auth / provider / network (LLM endpoint or gh)
        "gh auth login",
        "bad credentials",
        "rate limit",
        "service unavailable",
        "internal server error",
        "connection refused",
        "network is unreachable",
        // host resource exhaustion
        "no space left",
        "cannot allocate memory",
        "too many open files",
        "resource temporarily unavailable",
        "worker died",
    ];
    let lower = err.to_ascii_lowercase();
    !INFRA_ERROR_SIGNATURES.iter().any(|sig| lower.contains(sig))
}

fn dispatch_prompt(item: &AssignedWorkItem) -> String {
    let task = match item.description.as_deref() {
        Some(desc) if !desc.trim().is_empty() => format!("{}\n\n{}", item.title, desc.trim()),
        _ => item.title.clone(),
    };
    // Retry-with-context (escalation ladder, stage 1): on a retry, feed the prior
    // failure back into the prompt so the model doesn't repeat the same mistake.
    // Previously last_error was stored but NEVER injected — a no-op. This closes it.
    let retry_context = match (item.attempts, item.last_error.as_deref()) {
        (n, Some(err)) if n > 0 && !err.trim().is_empty() && retry_error_is_actionable(err) => {
            format!(
                "\n\n⚠ This is retry attempt {}. The previous attempt(s) FAILED with:\n{}\n\
                 Diagnose why it failed and fix THAT root cause — do not repeat the same approach.\n",
                n + 1,
                err.trim()
            )
        }
        // Retry after an INFRASTRUCTURE failure (spawn / heartbeat / backend /
        // pool / timeout) the coding agent cannot fix. Injecting that error plus
        // "diagnose the root cause" actively sabotages the retry — the agent
        // burns the attempt trying to "fix" e.g. "no dispatchable backend".
        // Acknowledge the retry without the unactionable error so it simply
        // re-approaches the task fresh.
        (n, Some(_)) if n > 0 => format!(
            "\n\n⚠ This is retry attempt {}. A prior attempt did not complete due to an \
             infrastructure issue (not your code) — approach the task fresh.\n",
            n + 1
        ),
        _ => String::new(),
    };
    format!(
        "Target repo:\n- project_id: {}\n- repo_url: {}\n- checkout: {}\n\n{}{}{}",
        item.project_id,
        item.repo_url.as_deref().unwrap_or("unknown"),
        item.repo_path.display(),
        task,
        retry_context,
        DISPATCH_HOUSE_RULES,
    )
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

    // Lane-1 health gate: the local codegen harness needs a local agent-capable
    // model; on nodes where none is viable it hangs and only fails over after
    // LANE1_TIMEOUT_SECS (~7min) — burned on EVERY build. Reuse the provider
    // circuit breaker under a synthetic provider so that once Lane 1 has failed
    // repeatedly ON THIS NODE the breaker opens and builds skip straight to the
    // cloud backstop; the breaker half-opens after a cooldown so a single probe
    // re-checks whether a capable local model has since come online.
    const LOCAL_CODEGEN_PROVIDER: &str = "local-codegen";
    let lane1_breaker_open =
        crate::circuit_breaker::is_provider_open(pg, item.computer_id, LOCAL_CODEGEN_PROVIDER)
            .await
            .unwrap_or(false);

    // Lane 1: local codegen harness — but skip it once we've escalated to cloud
    // (stage 2, after ESCALATE_TO_CLOUD_AT local failures it has failed the same
    // way) OR when this node's local-codegen breaker is open (it's been failing).
    if item.attempts < ESCALATE_TO_CLOUD_AT && !lane1_breaker_open {
        // Bound Lane 1 with a hard timeout so a hung local codegen harness fails
        // OVER to the cloud backstop instead of wedging the slot forever (see
        // LANE1_TIMEOUT_SECS). Without this, a hang here stalls the build while
        // the outer heartbeat keeps the lease alive — unrecoverable.
        let lane1 = tokio::time::timeout(
            Duration::from_secs(LANE1_TIMEOUT_SECS),
            crate::codegen_apply::codegen_apply(pg, &worktree.worktree_path, &prompt, None, 4),
        )
        .await;
        // Feed every Lane-1 outcome back into the breaker so it opens after a run
        // of failures on this node and closes again once it lands.
        let lane1_failed = |cat: &'static str| {
            let pg = pg.clone();
            let cid = item.computer_id;
            async move {
                let _ = crate::circuit_breaker::record_provider_failure(
                    &pg,
                    cid,
                    LOCAL_CODEGEN_PROVIDER,
                    cat,
                )
                .await;
            }
        };
        match lane1 {
            Ok(Ok(outcome)) if outcome.applied => {
                let _ = crate::circuit_breaker::record_provider_success(
                    pg,
                    item.computer_id,
                    LOCAL_CODEGEN_PROVIDER,
                )
                .await;
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
            Ok(Ok(outcome)) => {
                lane1_failed("local_codegen_unavailable").await;
                info!(
                    work_item_id = %item.work_item_id,
                    error = ?outcome.error,
                    "work_item_dispatch: local codegen didn't land; backstop to codex"
                );
            }
            Ok(Err(e)) => {
                lane1_failed("local_codegen_unavailable").await;
                warn!(
                    work_item_id = %item.work_item_id,
                    // Full anyhow chain so the REAL cause surfaces (e.g. the underlying
                    // fleet_oneshot failure), not just the "fleet_oneshot round 1" wrapper.
                    error = format!("{e:#}"),
                    "work_item_dispatch: local codegen errored; backstop to codex"
                );
            }
            Err(_) => {
                lane1_failed("local_codegen_unavailable").await;
                warn!(
                    work_item_id = %item.work_item_id,
                    timeout_secs = LANE1_TIMEOUT_SECS,
                    "work_item_dispatch: local codegen TIMED OUT (hung) — backstop to codex"
                );
            }
        }
    } else if lane1_breaker_open {
        info!(
            work_item_id = %item.work_item_id,
            "work_item_dispatch: local-codegen breaker OPEN on this node — skipping Lane 1, straight to cloud backstop"
        );
    } else {
        info!(
            work_item_id = %item.work_item_id,
            attempts = item.attempts,
            "work_item_dispatch: escalated (stage 2) — skipping local lane, straight to cloud backstop"
        );
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
    let forced_backend = primary_or_default_backend(&backends);
    let mut attempted_backend = false;
    let mut last_output: Option<(String, Output)> = None;
    // Capture EVERY backend's error so a total failure surfaces WHY for ALL of
    // them (codex + claude + kimi) in the DB `last_error` — not just the last —
    // ending the SSH-into-node log-diving needed to see codex/claude when only
    // kimi (the last tried) was recorded. Each error is tail-trimmed since the
    // full command echoes the huge prompt; the status/stderr lives at the end.
    let mut backend_errors: Vec<String> = Vec::new();

    for backend in &backends {
        if crate::circuit_breaker::is_provider_open(pg, computer_id, backend)
            .await
            .unwrap_or(false)
        {
            info!(backend = %backend, "run_ff_dispatch: skipping breaker-open backend");
            continue;
        }
        attempted_backend = true;
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
                    backend_errors.push(format!("{backend}: {}", err_tail(&format!("{e:#}"))));
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
    if !attempted_backend {
        warn!(
            backend = %forced_backend,
            "run_ff_dispatch: all routed backends were skipped before launch; forcing one direct attempt"
        );
        match run_backend_cli(&forced_backend, &worktree.worktree_path, &prompt).await {
            Ok(out) => return Ok((forced_backend, out)),
            Err(e) => {
                if worktree_has_diff(&worktree.worktree_path) {
                    warn!(backend = %forced_backend, error = %e, "run_ff_dispatch: forced backend timed out but wrote a real diff — salvaging");
                    let _ = crate::circuit_breaker::record_provider_success(
                        pg,
                        computer_id,
                        &forced_backend,
                    )
                    .await;
                    return Ok((
                        forced_backend,
                        synthetic_output("salvaged diff after forced backend timeout"),
                    ));
                }
                return Err(e);
            }
        }
    }
    last_output.map(Ok).unwrap_or_else(|| {
        if backend_errors.is_empty() {
            // Genuinely nothing to run: every backend was breaker-open / skipped.
            bail!(
                "run_ff_dispatch: no dispatchable backend on this node (all backends breaker-open or skipped)"
            )
        } else {
            // Every attempted backend errored — surface ALL of their causes (the
            // dispatch_prompt classifier still treats infra errors as
            // non-actionable on retry; this is for the operator + DB).
            bail!(
                "run_ff_dispatch: all backends failed on this node:\n{}",
                backend_errors.join("\n")
            )
        }
    })
}

fn primary_or_default_backend(backends: &[String]) -> String {
    backends
        .first()
        .cloned()
        .unwrap_or_else(|| "codex".to_string())
}

/// The backend name for interaction attribution when a dispatch errored before
/// any backend produced output (so `run_ff_dispatch` returned Err, carrying no
/// backend). Best-effort: the first routed backend, else the historical default.
async fn primary_dispatch_backend(pg: &PgPool, computer_id: Uuid) -> String {
    ff_db::pg_routed_backends(pg, computer_id, 5400)
        .await
        .ok()
        .map(|b| primary_or_default_backend(&b))
        .unwrap_or_else(|| "codex".to_string())
}

/// Council cap: how many headless auto-continue re-injections to attempt on a
/// transient cloud error before switching providers.
const AUTO_CONTINUE_MAX: u32 = 2;

/// Run `ff cli <backend>` against the worktree once and capture its Output.
/// A persistent cargo target dir for the sub-agent SLOT that owns `worktree_cwd`
/// (`.../sub-agents/sub-agent-N/worktrees/wi/XXX` → `.../sub-agent-N/cargo-target`).
/// Keeps the compile cache warm across a slot's tasks so verification builds are
/// incremental, while staying per-slot so concurrent slots don't fight over one
/// cargo lock. Falls back to a single shared node cache if the path doesn't match
/// the sub-agent layout (e.g. a differently-rooted checkout).
fn slot_cargo_target(worktree_cwd: &Path) -> PathBuf {
    for anc in worktree_cwd.ancestors() {
        if anc
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("sub-agent"))
            .unwrap_or(false)
        {
            return anc.join("cargo-target");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("cargo-shared-target")
}

async fn run_backend_cli(backend: &str, cwd: &Path, prompt: &str) -> Result<Output> {
    let backend = backend.to_string();
    let cwd = cwd.to_path_buf();
    let prompt = prompt.to_string();
    // Fetch the GitHub token HERE (async) and inject it into the backend's env so
    // the agent has an authenticated `gh` for the ENTIRE build — not only the
    // final `gh pr create` step. Without it, a codex/claude/kimi run that shells
    // out to `gh` mid-build hits "To get started with GitHub CLI, run gh auth
    // login" and exits non-zero on any node lacking ambient gh auth (i.e. all of
    // them — the fleet authenticates gh purely via this secret, not `gh auth login`).
    let gh_token = crate::fleet_info::fetch_secret("github_gh_token").await;
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
        if let Some(token) = gh_token {
            cmd.env("GH_TOKEN", token);
        }
        // Point cargo at a PERSISTENT per-slot target dir so a `cargo check` the
        // agent runs to verify its change is INCREMENTAL (seconds) instead of a
        // cold from-scratch compile of the whole workspace (many minutes) — each
        // sub-agent worktree is a fresh checkout with an EMPTY target/, which made
        // compile-heavy feature tasks blow past the dispatch timeout (the "codex
        // hangs, 0 PRs" symptom — the 8 stuck procs were rustc/cargo). Per-slot
        // (not one shared dir) keeps concurrent slots from serializing on cargo's
        // target lock; it warms up on the slot's first build and stays warm after.
        cmd.env("CARGO_TARGET_DIR", slot_cargo_target(&cwd));
        run_command_timeout(cmd, Duration::from_secs(FF_TIMEOUT_SECS + 30))
    })
    .await
    .context("join ff dispatch task")?
}

/// Runs `git status --porcelain` and returns true if the worktree has any
/// uncommitted change, including tracked edits or untracked files.
/// Used to SALVAGE a backend that wrote a valid diff but timed out before
/// exiting; the work is real even though the process didn't terminate cleanly.
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
    // Fetch origin/<base> so the worktree starts from the TRUE latest main, not a
    // stale local ref. A SILENT fetch failure + the old stale-local fallback below
    // were the #1 cause of entangled/unmergeable fleet PRs (2026-07-04: builds
    // branched from an old base and re-added an already-merged fold). Retry, and
    // FAIL rather than branch from a possibly-stale base — a failed build retries
    // cleanly; a stale-base build produces a garbage PR.
    let mut fetched = false;
    for attempt in 0..3 {
        match run_git(
            repo_path,
            ["fetch", "origin", base_branch],
            Duration::from_secs(120),
        ) {
            Ok(_) => {
                fetched = true;
                break;
            }
            Err(e) => {
                warn!(base_branch, attempt, error = %e, "git_worktree_add: fetch origin failed; retrying")
            }
        }
    }
    if !fetched {
        bail!(
            "git_worktree_add: could not fetch origin/{base_branch} in 3 tries — \
             refusing to branch the worktree from a possibly-stale base"
        );
    }

    // Branch STRICTLY from the freshly-fetched origin/<base>. No stale-local
    // fallback: if this fails (e.g. a leftover worktree/branch), bail so the build
    // retries from a clean slate rather than silently building on the local ref.
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
    )?;
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

/// Render a command for logs as `program arg1 arg2 …`, deliberately EXCLUDING
/// the environment. `{Command:?}` prints env vars too, so it would leak any
/// secret injected via `.env()` (e.g. the `GH_TOKEN` the dispatch sets for the
/// backend build) into the daemon log on every failed/timed-out command.
fn command_display(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        s.push(' ');
        s.push_str(&arg.to_string_lossy());
    }
    truncate_for_log(&s)
}

fn run_command_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let program = command_display(&cmd);
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

/// The TAIL of a long backend-run error. `run_backend_cli`'s error leads with the
/// full command (which echoes the entire prompt), so the useful part —
/// `status: … stderr: …` — sits at the END. Keep the last ~280 chars so each
/// per-backend line in `last_error` shows the real failure, not the prompt.
fn err_tail(s: &str) -> String {
    const KEEP: usize = 280;
    let count = s.chars().count();
    if count <= KEEP {
        return s.trim().to_string();
    }
    let tail: String = s.chars().skip(count - KEEP).collect();
    format!("…{}", tail.trim())
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

/// `(available_gb, used_pct)` for the filesystem holding `path`, via `df -Pk`
/// (POSIX one-line-per-fs, portable across macOS/Linux). None on any parse error.
fn disk_free_for(path: &Path) -> Option<(f64, f64)> {
    let out = Command::new("df").arg("-Pk").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let cols: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    // Filesystem 1024-blocks Used Available Capacity Mounted-on
    let avail_kb: f64 = cols.get(3)?.parse().ok()?;
    let pct: f64 = cols.get(4)?.trim_end_matches('%').parse().ok()?;
    Some((avail_kb / 1024.0 / 1024.0, pct))
}

/// Prune the persistent per-slot cargo caches (see [`slot_cargo_target`]) when
/// the disk is getting TIGHT, so warm build caches can't silently fill it. Only
/// reaps under pressure (>90% used or <15 GB free) — otherwise the caches stay
/// warm. Removes least-recently-modified caches first until back under a comfort
/// margin. Best-effort + sync (run under spawn_blocking): fs walk + `rm -rf`.
fn reap_cargo_targets() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let sub_agents = home.join(".forgefleet").join("sub-agents");
    match disk_free_for(&sub_agents) {
        Some((avail_gb, use_pct)) if use_pct >= 90.0 || avail_gb <= 15.0 => {}
        _ => return, // healthy or unknown → leave the warm caches alone
    }

    let mut caches: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&sub_agents) {
        for slot in rd.flatten() {
            let ct = slot.path().join("cargo-target");
            if ct.is_dir() {
                let mtime = ct
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                caches.push((ct, mtime));
            }
        }
    }
    caches.sort_by_key(|(_, t)| *t); // least-recently-modified first

    for (ct, _) in caches {
        // Stop once there's a comfortable margin again.
        if let Some((avail_gb, use_pct)) = disk_free_for(&sub_agents)
            && use_pct < 85.0
            && avail_gb > 25.0
        {
            break;
        }
        warn!(dir = %ct.display(), "cargo_target_reaper: disk tight — removing a warm cargo cache to free space");
        let _ = std::fs::remove_dir_all(&ct);
    }
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
                    // Keep the persistent per-slot cargo caches from filling the
                    // disk — prunes only under disk pressure, else leaves them warm.
                    let _ = tokio::task::spawn_blocking(reap_cargo_targets).await;
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
    use super::{
        DispatchOutcome, classify_dispatch_outcome, command_display, dispatch_budget_for_host,
        parse_cli_tokens, primary_or_default_backend, retry_error_is_actionable,
    };

    #[test]
    fn retry_error_is_actionable_suppresses_infra_injects_task_errors() {
        // Infra failures the coding agent can't fix → must NOT be injected.
        for infra in [
            "run_ff_dispatch: no dispatchable backend on this node",
            "run_ff_dispatch: all backends failed on this node:\ncodex: exit status: 1",
            "spawn \"ff\" \"cli\" \"codex\"",
            "stale-heartbeat takeover (attempt 2)",
            "fleet_oneshot round 3: route deployments: Postgres error: pool timed out",
            "command timed out after 1080s",
            "To get started with GitHub CLI, please run:  gh auth login",
        ] {
            assert!(
                !retry_error_is_actionable(infra),
                "infra error should be suppressed: {infra}"
            );
        }
        // Real task-level failures the agent CAN act on → must be injected.
        for task in [
            "error[E0433]: failed to resolve: use of undeclared crate or module `foo`",
            "test result: FAILED. 1 passed; 1 failed",
            "cannot find function `backend_rank` in this scope",
            "assertion `left == right` failed",
        ] {
            assert!(
                retry_error_is_actionable(task),
                "task error should be injected: {task}"
            );
        }
    }
    use anyhow::anyhow;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Output};

    #[test]
    fn slot_cargo_target_is_per_slot_with_shared_fallback() {
        let cwd =
            std::path::Path::new("/home/x/.forgefleet/sub-agents/sub-agent-2/worktrees/wi/abc123");
        assert_eq!(
            super::slot_cargo_target(cwd),
            std::path::Path::new("/home/x/.forgefleet/sub-agents/sub-agent-2/cargo-target")
        );
        // A path that doesn't match the sub-agent layout → shared node cache.
        assert!(
            super::slot_cargo_target(std::path::Path::new("/tmp/random/dir"))
                .ends_with("cargo-shared-target")
        );
    }

    #[test]
    fn err_tail_keeps_the_end_where_status_and_stderr_live() {
        let short = "codex: exit status: 4 stderr: gh auth login";
        assert_eq!(super::err_tail(short), short);
        // A long error (command echoes the prompt up front) keeps the END.
        let long = format!(
            "{}\nstatus: exit status: 1\nstderr: LLM not set",
            "x".repeat(1000)
        );
        let tail = super::err_tail(&long);
        assert!(
            tail.contains("LLM not set"),
            "tail must retain stderr: {tail}"
        );
        assert!(tail.len() < 320);
    }

    #[test]
    fn command_display_never_leaks_env_secrets() {
        // Regression guard: a secret injected via .env() (e.g. GH_TOKEN) must
        // NEVER appear in the command's log rendering. `{Command:?}` would leak
        // it; command_display renders program + args only.
        let mut cmd = Command::new("ff");
        cmd.args(["cli", "codex", "--cwd", "/tmp/wt"])
            .env("GH_TOKEN", "gho_supersecretvalue_should_never_be_logged");
        let shown = command_display(&cmd);
        assert!(shown.contains("ff cli codex"));
        assert!(
            !shown.contains("gho_supersecretvalue_should_never_be_logged"),
            "command_display leaked an env secret: {shown}"
        );
        assert!(!shown.contains("GH_TOKEN"), "env var name leaked: {shown}");
    }

    fn ok_output() -> anyhow::Result<Output> {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"done".to_vec(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn classify_success_on_ok() {
        assert_eq!(
            classify_dispatch_outcome(&ok_output(), false),
            DispatchOutcome::Success
        );
        // Ok is Success regardless of diff.
        assert_eq!(
            classify_dispatch_outcome(&ok_output(), true),
            DispatchOutcome::Success
        );
    }

    #[test]
    fn classify_failed_no_diff_is_retryable_class() {
        let r: anyhow::Result<Output> = Err(anyhow!("codex exited 1"));
        assert_eq!(
            classify_dispatch_outcome(&r, false),
            DispatchOutcome::FailedNoDiff
        );
    }

    #[test]
    fn classify_failed_with_diff_when_error_but_diff_present() {
        let r: anyhow::Result<Output> = Err(anyhow!("codex exited 1: patch apply error"));
        assert_eq!(
            classify_dispatch_outcome(&r, true),
            DispatchOutcome::FailedWithDiff
        );
    }

    #[test]
    fn classify_timeout_salvaged_when_timeout_and_diff() {
        let r: anyhow::Result<Output> = Err(anyhow!("command timed out after 1080s"));
        assert_eq!(
            classify_dispatch_outcome(&r, true),
            DispatchOutcome::TimeoutSalvaged
        );
        // Timeout with NO diff is still just FailedNoDiff (nothing to salvage).
        let r2: anyhow::Result<Output> = Err(anyhow!("command timed out after 1080s"));
        assert_eq!(
            classify_dispatch_outcome(&r2, false),
            DispatchOutcome::FailedNoDiff
        );
    }

    #[test]
    fn budget_zero_free_slots_is_zero() {
        assert_eq!(dispatch_budget_for_host(0, 0), 0);
        assert_eq!(dispatch_budget_for_host(0, 10), 0);
    }

    #[test]
    fn budget_scales_with_free_slots_up_to_cap() {
        // 1 free slot → 1; 2 → 2; at/over the cap (3) → capped at 3.
        assert_eq!(dispatch_budget_for_host(1, 0), 1);
        assert_eq!(dispatch_budget_for_host(2, 0), 2);
        assert_eq!(dispatch_budget_for_host(3, 0), 3);
        assert_eq!(dispatch_budget_for_host(50, 0), 3);
    }

    #[test]
    fn budget_backpressure_throttles_to_one() {
        // Plenty of free slots but a failing host → throttle to 1.
        assert_eq!(dispatch_budget_for_host(50, 3), 1);
        assert_eq!(dispatch_budget_for_host(50, 99), 1);
        // Below the threshold → no throttle.
        assert_eq!(dispatch_budget_for_host(50, 2), 3);
    }

    #[test]
    fn parses_codex_tokens_used_with_comma() {
        // The exact shape codex prints.
        assert_eq!(parse_cli_tokens("codex\nHELLO\ntokens used\n9,332\n"), 9332);
    }

    #[test]
    fn parses_tokens_used_with_spaces_and_large_comma_number() {
        assert_eq!(parse_cli_tokens("tokens used  1,234,567"), 1234567);
    }

    #[test]
    fn parses_uppercase_tokens_used_followed_by_zero() {
        assert_eq!(parse_cli_tokens("TOKENS USED\n0"), 0);
    }

    #[test]
    fn prefers_tokens_used_marker_over_earlier_used_number() {
        assert_eq!(parse_cli_tokens("used 5 tokens then tokens used\n99"), 99);
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

    #[test]
    fn forced_fallback_prefers_first_routed_backend() {
        assert_eq!(
            primary_or_default_backend(&["kimi".to_string(), "codex".to_string()]),
            "kimi"
        );
    }

    #[test]
    fn forced_fallback_defaults_to_codex_when_unrouted() {
        assert_eq!(primary_or_default_backend(&[]), "codex");
    }
}
