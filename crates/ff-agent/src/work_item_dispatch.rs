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
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
#[cfg(test)]
use tokio::sync::{Semaphore, SemaphorePermit};
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
/// Dispatch-owned wall-clock ceiling. Unlike the heartbeat reaper, this fires
/// even while a wedged build is successfully refreshing its lease. Keep the
/// scheduler's max-lease-age sweep longer as a last-resort recovery path for a
/// crashed dispatcher.
// 40 min: the dispatch-level build watchdog. Was 20 min (1200s), which killed legitimate
// multi-round local-model builds + cold Rust compiles on the large workspace (the recurring
// "max-build-duration exceeded after 1200s" failures). Kept generous since a genuinely hung
// build is caught earlier by the stalled-attempts + heartbeat reaper.
const MAX_BUILD_DURATION_SECS: u64 = 40 * 60;
const _: () =
    assert!(MAX_BUILD_DURATION_SECS < crate::work_item_scheduler::MAX_LEASE_DURATION_SECS as u64);
const MAX_SELF_FIX_ATTEMPTS: usize = 1;
/// Ceiling on how many work_items a single host starts in one dispatch tick.
/// The effective budget per tick is [`dispatch_budget_for_host`], which scales
/// with the host's free sub-agent slots up to this cap (and drops to 1 under
/// backpressure). Replaces the old hard `1/tick`, which left the fleet mostly
/// idle even with many ready tasks and dozens of free slots.
pub(crate) const MAX_DISPATCH_PER_TICK: i64 = 3;

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
    computer_name: String,
    session_id: Option<Uuid>,
    slot: i32,
    kind: String,
    /// Prior failed attempts (escalation ladder). Drives backend escalation
    /// (local → cloud) and prompt context injection.
    attempts: i32,
    /// The error from the previous attempt, fed back into this attempt's prompt
    /// so the model doesn't repeat the same mistake (retry-with-context).
    last_error: Option<String>,
    /// Task complexity from the decomposer (`mechanical` | `moderate` | `complex`).
    /// Mechanical and moderate tasks try the cheap local Lane-1 codegen harness
    /// first; complex tasks and any task predicted to touch more than
    /// [`PREDICTED_PATHS_CLOUD_THRESHOLD`] files skip straight to the cloud CLI
    /// backstop (see [`AssignedWorkItem::prefers_cloud_lane`]).
    complexity: String,
    /// How many files the decomposer predicted this task touches. A multi-file
    /// change is beyond the local codegen harness's reliable range, so it too
    /// routes straight to cloud.
    predicted_paths_count: i32,
    /// Precomputed Cortex context stored on the `work_items` row. Loaded at
    /// dispatch time so the prompt can point the agent at the exact symbols to
    /// touch without recomputing them via `ff cortex find`.
    brain_node_ids: Vec<String>,
    /// Precomputed file paths stored on the `work_items` row, companion to
    /// `brain_node_ids`.
    touched_paths: Vec<String>,
    /// Detector-authored evidence plus Cortex/brain discovery hints.
    context: serde_json::Value,
    /// Ordered lifecycle steps. The agent executes these around the leaf work.
    pre_work: Vec<String>,
    work: Vec<String>,
    post_work: Vec<String>,
}

impl AssignedWorkItem {
    /// Whether this task should SKIP the local Lane-1 codegen harness and go
    /// straight to the cloud CLI (codex/claude/kimi). The local lane is great for
    /// small mechanical and moderate edits, but complex or multi-file-heavy work
    /// (predicted to touch more than [`PREDICTED_PATHS_CLOUD_THRESHOLD`] files)
    /// can hang or half-finish there (observed 2026-07-06: a 3-file React feature
    /// wedged a slot for 24min producing nothing). Route those to the more capable
    /// cloud lane from attempt 0 instead of burning a wedge-prone local attempt
    /// first.
    fn prefers_cloud_lane(&self) -> bool {
        task_prefers_cloud_lane(&self.complexity, self.predicted_paths_count)
    }
}

/// Number of predicted touched paths above which a task is treated as
/// "multi-file-heavy" and routed straight to the cloud CLI, even when its
/// complexity is `mechanical` or `moderate`. Chosen so the local Lane-1 harness
/// gets the majority of moderate single/double-file edits while avoiding the
/// multi-file wedge class that local codegen fumbles.
const PREDICTED_PATHS_CLOUD_THRESHOLD: i32 = 3;

/// Pure routing predicate (unit-testable): a task skips the local Lane-1 codegen
/// harness for the cloud CLI when it's `complex` OR predicted to touch more than
/// [`PREDICTED_PATHS_CLOUD_THRESHOLD`] files. Mechanical and moderate tasks that
/// are not multi-file-heavy stay on the cheap local lane.
fn task_prefers_cloud_lane(complexity: &str, predicted_paths_count: i32) -> bool {
    matches!(complexity, "complex") || predicted_paths_count > PREDICTED_PATHS_CLOUD_THRESHOLD
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

    // Keep this computer's canonical slot row in sync with the local filesystem:
    // disabled when the tree is dirty or an operator session owns it, idle otherwise.
    if let Some(computer_id) =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM computers WHERE name = $1")
            .bind(worker_name)
            .fetch_optional(pg)
            .await
            .ok()
            .flatten()
    {
        let project = crate::agent_coordinator::canonical_project_name();
        let _ = crate::agent_coordinator::ensure_canonical_sub_agent_row(
            pg,
            computer_id,
            &project,
            Some(worker_name),
        )
        .await;
    }

    // Bump the dispatch tick for every active lease on this host. The stale-lease
    // reaper watches `dispatch_tick_at` independently of `heartbeat_at` so it can
    // reclaim a lease whose host dispatch loop wedged even though the in-build
    // process keeps heartbeating.
    bump_dispatch_tick_at(pg, worker_name).await;

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

    // Only ensure on-disk sub-agent directories for regular slots. Canonical
    // slots use an existing project checkout (~/projects/{project}) and must not
    // have a synthetic sub-agent-N directory created for them.
    let regular_slots: Vec<_> = assigned.iter().filter(|w| w.kind != "canonical").collect();
    if !regular_slots.is_empty() {
        let max_slot = regular_slots
            .iter()
            .map(|w| w.slot)
            .max()
            .unwrap_or(0)
            .max(0) as u32;
        ensure_workspaces(max_slot + 1).map_err(|e| anyhow!("ensure sub-agent workspaces: {e}"))?;
    }

    // SPAWN each dispatch so a multi-minute build doesn't BLOCK this tick — the
    // handler awaits evaluate_work_item_dispatch, so a serial `.await` per item
    // froze the whole dispatch loop for the FIRST build's duration while every
    // OTHER assigned lease on this host stale-reaped at LEASE_STALE_SECS (#72:
    // duncan got 3 leases, ran 1, the other 2 reaped). Each spawned dispatch
    // claims 'building' first (see dispatch_one), so the next tick's
    // assigned_work_items excludes in-flight items → no double-dispatch; the
    // budget above already caps how many start per tick to this host's free slots.
    let started = assigned.len();
    for item in assigned {
        let pg = pg.clone();
        let worker_name = worker_name.to_string();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(
                Duration::from_secs(MAX_BUILD_DURATION_SECS),
                dispatch_one(pg.clone(), item.clone(), worker_name),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "max-build-duration exceeded after {}s; build cancelled",
                    MAX_BUILD_DURATION_SECS
                )),
            };
            if let Err(e) = result {
                if is_build_timeout(&e) {
                    warn!(
                        work_item_id = %item.work_item_id,
                        sub_agent_id = %item.sub_agent_id,
                        attempts = item.attempts + 1,
                        error = %e,
                        "work_item_dispatch: build timed out; requeueing for retry"
                    );
                } else {
                    warn!(
                        work_item_id = %item.work_item_id,
                        sub_agent_id = %item.sub_agent_id,
                        error = %e,
                        "work_item_dispatch: dispatch failed"
                    );
                }
                if let Err(cleanup) = requeue_or_fail(&pg, &item, &e.to_string()).await {
                    warn!(
                        work_item_id = %item.work_item_id,
                        error = %cleanup,
                        "work_item_dispatch: failure cleanup failed"
                    );
                }
            }
        });
    }

    if started > 0 {
        info!(
            started,
            "work_item_dispatch: started assigned work_items (concurrent)"
        );
    }
    Ok(started)
}

/// Whether a dispatch error was caused by a build command exceeding its time
/// budget. Inspect the full anyhow chain because callers add context while the
/// timeout itself is normally the innermost error.
fn is_build_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("timed out")
            || message.contains("timeout")
            || message.contains("max-build-duration exceeded")
    })
}

/// Spawn the dispatch loop. PER-HOST (not leader-gated): the scheduler (leader)
/// assigns work_items to slots on ANY host, and each host must execute ITS OWN
/// slots. `evaluate_work_item_dispatch` is host-scoped (`c.name = worker_name`),
/// so running it on every host dispatches only that host's assigned slots.
pub fn spawn_work_item_dispatch(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    pulse_tick_at_unix: Arc<AtomicU64>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = Instant::now();
        let last_tick_at = Arc::new(AtomicU64::new(start.elapsed().as_secs()));
        let watchdog_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(crate::daemon::dispatch_tick_watchdog(
            start,
            last_tick_at.clone(),
            watchdog_shutdown_rx,
        ));

        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    last_tick_at.store(start.elapsed().as_secs(), Ordering::Relaxed);
                    pulse_tick_at_unix.store(chrono::Utc::now().timestamp() as u64, Ordering::Relaxed);
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

/// Sanitized repo name derived from a clone URL. Shared by the per-slot clone
/// path and the fleet artifact cache so a cache lookup uses the same key as
/// the destination clone.
fn repo_slug(repo_url: &str) -> String {
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
    if slug.is_empty() {
        "repo".to_string()
    } else {
        slug.to_string()
    }
}

/// Clone location for a work_item's repo: INSIDE the assigned sub-agent slot, so
/// each slot holds its OWN full checkout (build-path option A, 2026-07-07) — e.g.
/// `~/.forgefleet/sub-agents/sub-agent-3/forge-fleet`. Replaces the old shared
/// top-level `~/.forgefleet/project-repos/{project}/{repo}`. `slot` is clamped to
/// ≥0. Stage 1 of the refactor: worktrees still branch off this clone (under the
/// same slot) until Stage 2 works in the clone directly.
fn default_clone_path(slot: i32, repo_url: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("sub-agents")
        .join(format!("sub-agent-{}", slot.max(0)))
        .join(repo_slug(repo_url))
}

/// Shared fleet artifact cache root for project repos.
fn artifact_cache_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("cache")
        .join("repos")
}

/// Cache directory for a repo URL. This is a single, shared mirror that each
/// per-slot clone can copy from instead of hitting the WAN.
fn repo_cache_path(repo_url: &str) -> PathBuf {
    artifact_cache_root().join(repo_slug(repo_url))
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
            COALESCE(NULLIF(w.complexity, ''), 'mechanical') AS complexity,
            COALESCE(jsonb_array_length(w.predicted_paths), 0) AS predicted_paths_count,
            COALESCE(w.brain_node_ids, '[]'::jsonb) AS brain_node_ids,
            COALESCE(w.touched_paths, '[]'::jsonb) AS touched_paths,
            COALESCE(w.context, '{}'::jsonb) AS context,
            COALESCE(w.pre_work, '[]'::jsonb) AS pre_work,
            COALESCE(w.work, '[]'::jsonb) AS work,
            COALESCE(w.post_work, '[]'::jsonb) AS post_work,
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
            sa.slot,
            sa.kind,
            sa.workspace_dir,
            c.name AS computer_name,
            l.session_id
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
            let kind: String = r.get("kind");
            let workspace_dir: Option<PathBuf> = r
                .try_get::<Option<String>, _>("workspace_dir")
                .ok()
                .flatten()
                .map(|p| PathBuf::from(expand_tilde(&p)));
            let local_bound_path = bound_repo_path.as_ref().filter(|p| p.exists()).cloned();
            // Clone-per-slot ALWAYS (operator decision 2026-07-17): when the
            // item has a repo_url, the workspace is the slot's own clone — the
            // ensure-clone step creates it on first dispatch. Bound/metadata
            // paths are only honored for repo-url-less items (nothing to clone
            // from), where the single shared path is all we have.
            // Exception: canonical slots (kind='canonical') use their configured
            // workspace_dir (e.g. ~/projects/forge-fleet) as the repo path.
            let repo_path = if kind == "canonical" {
                workspace_dir.clone().unwrap_or_else(|| {
                    repo_url
                        .as_deref()
                        .map(|url| default_clone_path(r.get::<i32, _>("slot"), url))
                        .unwrap_or_else(|| PathBuf::from(&fallback_repo_path))
                })
            } else {
                repo_url
                    .as_deref()
                    .map(|url| default_clone_path(r.get::<i32, _>("slot"), url))
                    .or(local_bound_path)
                    .or(metadata_repo_path)
                    .or(bound_repo_path)
                    .unwrap_or_else(|| PathBuf::from(fallback_repo_path))
            };
            Ok(AssignedWorkItem {
                work_item_id: r.get("work_item_id"),
                project_id: r.get("project_id"),
                title: r.get("title"),
                description: r.try_get("description").ok().flatten(),
                base_branch: r.try_get("base_branch").ok().flatten(),
                repo_id: r.try_get("repo_id").ok().flatten(),
                repo_url,
                repo_path,
                kind,
                sub_agent_id: r.get("sub_agent_id"),
                computer_id: r.get("computer_id"),
                computer_name: r.get("computer_name"),
                session_id: r.try_get("session_id").ok().flatten(),
                slot: r.get("slot"),
                attempts: r.try_get("attempts").unwrap_or(0),
                last_error: r.try_get("last_error").ok().flatten(),
                complexity: r
                    .try_get("complexity")
                    .unwrap_or_else(|_| "mechanical".to_string()),
                predicted_paths_count: r.try_get("predicted_paths_count").unwrap_or(0),
                brain_node_ids: jsonb_string_array(&r, "brain_node_ids"),
                touched_paths: jsonb_string_array(&r, "touched_paths"),
                context: r
                    .try_get("context")
                    .unwrap_or_else(|_| serde_json::json!({})),
                pre_work: jsonb_string_array(&r, "pre_work"),
                work: jsonb_string_array(&r, "work"),
                post_work: jsonb_string_array(&r, "post_work"),
            })
        })
        .collect()
}

/// Parse a JSONB array column into a `Vec<String>`, skipping non-string values.
/// Empty arrays and missing columns resolve to an empty vector so dispatch can
/// fall back to the legacy Cortex lookup.
fn jsonb_string_array(row: &sqlx::postgres::PgRow, column: &str) -> Vec<String> {
    let value: serde_json::Value = match row.try_get(column) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match value {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) if !s.trim().is_empty() => Some(s),
                _ => None,
            })
            .collect(),
        serde_json::Value::String(s) if !s.trim().is_empty() => vec![s],
        _ => Vec::new(),
    }
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

    // Clone via the CANONICAL GitHub identity, not the bare `git@github.com:`
    // host. The bare host resolves (per node's ~/.ssh/config) to a default key
    // that is UNAUTHORIZED on the venkatyarl account on most fleet nodes —
    // measured 2026-07-06: bare `git@github.com:` gets "Permission denied
    // (publickey)" on 9 of 14 workers, so every dispatch there died at clone
    // before any build ran. The canonical alias (`github.com-venkat` →
    // `id_venkat`, flagged in `github_ssh_aliases.is_canonical` by V161)
    // authenticates fleet-wide. We rewrite the URL host (so the persisted
    // `origin` remote — and every later fetch/push from worktrees — also uses
    // the authorized identity) AND force the identity via GIT_SSH_COMMAND so it
    // works even on a node whose ~/.ssh/config lacks the alias Host block.
    let (clone_url, ssh_identity) = match ff_db::pg_canonical_github_alias(pg).await {
        Ok(Some((alias, identity_file))) => (
            rewrite_github_host_alias(&github_url, &alias),
            Some(identity_file),
        ),
        Ok(None) => (github_url.clone(), None),
        Err(e) => {
            warn!(error = %e, "canonical github alias lookup failed; cloning with bare URL");
            (github_url.clone(), None)
        }
    };

    let repo_path = item.repo_path.clone();
    let cache_path = repo_cache_path(&github_url);

    // Try the fleet artifact cache first to avoid a WAN clone.
    if cache_path.join(".git").exists() {
        info!(
            work_item_id = %item.work_item_id,
            project_id = %item.project_id,
            repo_path = %repo_path.display(),
            cache_path = %cache_path.display(),
            "work_item_dispatch: staging repo from local artifact cache"
        );

        let clone_url_for_remote = clone_url.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = repo_path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create repo parent {}", parent.display()))?;
            }

            let mut cmd = Command::new("git");
            cmd.arg("clone")
                .arg("--local")
                .arg("--no-hardlinks")
                .arg(&cache_path)
                .arg(&repo_path);
            run_command_timeout(cmd, Duration::from_secs(300)).with_context(|| {
                format!(
                    "clone from cache {} into {}",
                    cache_path.display(),
                    repo_path.display()
                )
            })?;

            run_git(
                &repo_path,
                ["remote", "set-url", "origin", &clone_url_for_remote],
                Duration::from_secs(60),
            )
            .with_context(|| format!("set origin to {clone_url_for_remote}"))?;

            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("join cache-clone task")??;

        return Ok(());
    }

    info!(
        work_item_id = %item.work_item_id,
        project_id = %item.project_id,
        repo_path = %repo_path.display(),
        clone_url = %clone_url,
        "work_item_dispatch: cache miss — cloning project repo from WAN"
    );

    let repo_path_for_clone = repo_path.clone();
    let clone_url_for_clone = clone_url.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = repo_path_for_clone
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create repo parent {}", parent.display()))?;
        }

        let mut cmd = Command::new("git");
        cmd.arg("clone")
            .arg(&clone_url_for_clone)
            .arg(&repo_path_for_clone);
        if let Some(identity) = &ssh_identity {
            cmd.env(
                "GIT_SSH_COMMAND",
                format!(
                    "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
                    expand_home(identity)
                ),
            );
        }
        run_command_timeout(cmd, Duration::from_secs(600))
    })
    .await
    .context("join git clone task")?
    .with_context(|| format!("clone project repo into {}", repo_path.display()))?;

    // Seed the artifact cache for the next dispatch, but don't fail the task if
    // the mirror update itself fails.
    if let Err(e) = seed_repo_cache(&repo_path, &cache_path, &clone_url).await {
        warn!(
            work_item_id = %item.work_item_id,
            project_id = %item.project_id,
            error = %e,
            "work_item_dispatch: failed to seed artifact cache"
        );
    }

    Ok(())
}

/// Seed the shared repo artifact cache from a freshly cloned slot checkout.
/// Failures are surfaced to the caller but intentionally do NOT fail the
/// dispatch — the cache is an optimization, not a hard requirement.
async fn seed_repo_cache(repo_path: &Path, cache_path: &Path, clone_url: &str) -> Result<()> {
    if cache_path.join(".git").exists() {
        return Ok(());
    }

    let repo_path = repo_path.to_path_buf();
    let cache_path = cache_path.to_path_buf();
    let clone_url = clone_url.to_string();

    tokio::task::spawn_blocking(move || {
        if let Some(parent) = cache_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create cache parent {}", parent.display()))?;
        }

        let mut cmd = Command::new("git");
        cmd.arg("clone")
            .arg("--local")
            .arg("--no-hardlinks")
            .arg(&repo_path)
            .arg(&cache_path);
        run_command_timeout(cmd, Duration::from_secs(300)).with_context(|| {
            format!(
                "seed cache from {} into {}",
                repo_path.display(),
                cache_path.display()
            )
        })?;

        run_git(
            &cache_path,
            ["remote", "set-url", "origin", &clone_url],
            Duration::from_secs(60),
        )
        .with_context(|| format!("set cache origin to {clone_url}"))?;

        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("join cache-seed task")?
}

/// Rewrite an scp-style `git@github.com:owner/repo` clone URL to use a specific
/// SSH host alias (e.g. `github.com-venkat`), so the clone authenticates with the
/// canonical identity instead of the bare-host default key (unauthorized on most
/// fleet nodes). Only the `git@github.com:` form is rewritten; any other shape
/// (https, an already-aliased host, a non-github remote) is returned unchanged.
fn rewrite_github_host_alias(url: &str, alias: &str) -> String {
    const BARE: &str = "git@github.com:";
    match url.strip_prefix(BARE) {
        Some(rest) => format!("git@{alias}:{rest}"),
        None => url.to_string(),
    }
}

/// Expand a leading `~/` in an identity-file path to `$HOME` so it can be passed
/// to `ssh -i` (which does not do tilde expansion itself). A bare `~` or any
/// non-tilde path is returned unchanged.
fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    }
}

async fn dispatch_one(pg: PgPool, item: AssignedWorkItem, worker_name: String) -> Result<()> {
    // CLAIM + heartbeat FIRST, before the (possibly slow cold-clone) checkout.
    // dispatch_one now runs CONCURRENTLY with this host's other assigned leases
    // (spawned by evaluate_work_item_dispatch — a serial `.await` here blocked the
    // whole dispatch tick, so batch-assigned leases stale-reaped before their turn
    // #72). Because the tick re-fires while this build runs, marking 'building'
    // immediately excludes this item from the next tick's assigned_work_items
    // (its `w.status = 'claimed'` filter) → no double-dispatch. Starting the
    // heartbeat guard now also keeps the lease fresh THROUGH the checkout, so a
    // multi-minute clone can't stale-reap the lease before the build even starts.
    if !mark_building(&pg, &item).await? {
        // A concurrent dispatch already claimed this item (status was no longer
        // 'claimed') — skip cleanly rather than double-dispatch or clobber it.
        info!(
            work_item_id = %item.work_item_id,
            "work_item_dispatch: already claimed by a concurrent dispatch — skipping"
        );
        return Ok(());
    }

    // Keep the lease heartbeat alive for the ENTIRE dispatch — the backend build
    // AND the commit/push/PR tail — via an RAII guard that stops it when
    // dispatch_one returns on ANY path. Previously the heartbeat stopped the
    // instant the backend CLI returned, so a slow `git push` / `gh pr create` on
    // a big diff ran with a frozen lease and the watchdog could reap it
    // mid-finalize as a "stale-heartbeat takeover".
    let _heartbeat_guard = HeartbeatGuard::spawn(item.work_item_id);

    ensure_repo_checked_out(&pg, &item).await?;
    let worktree = create_worktree_for_item(&pg, &item).await?;

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

    // Preserve a tail of the agent's OWN output so a no-diff outcome below is
    // diagnosable (froze? errored? hung on a tool call? claimed done without
    // editing?) straight from the daemon log — no host repro needed (#69).
    let agent_output_tail = match dispatch_result {
        Ok(output) => {
            info!(
                work_item_id = %item.work_item_id,
                stdout_len = output.stdout.len(),
                stderr_len = output.stderr.len(),
                "work_item_dispatch: ff dispatch completed"
            );
            agent_output_tail(&output, 1500)
        }
        Err(e) => {
            mark_worktree_failed(&pg, item.work_item_id, &e.to_string()).await?;
            remove_worktree(&item.repo_path, &worktree.worktree_path)?;
            return Err(e);
        }
    };

    // codex (and most CLI agents) EDIT files but don't `git commit`. Commit any
    // changes it made in the worktree so they can become a PR. A clean worktree
    // (agent made no change) commits nothing → handled as "no commits" below.
    let (git_name, git_email) = resolve_git_identity(&pg, &item.project_id).await;
    let dirty =
        commit_worktree_changes(&worktree.worktree_path, &item.title, &git_name, &git_email)?;
    info!(
        work_item_id = %item.work_item_id, dirty,
        "work_item_dispatch: committed agent changes (dirty={dirty})"
    );

    let mut has_commits = branch_has_commits(
        &item.repo_path,
        &worktree.base_branch,
        &worktree.task_branch,
    )?;
    // SALVAGE an agent that committed its OWN work to a detached / self-made HEAD
    // instead of leaving it on the task branch (some codex runs do this despite
    // the house rule — see #801/#69). branch_has_commits only sees the task
    // branch, so it misses that; but if the WORKTREE HEAD is ahead of base the
    // real work IS there. Adopt it onto the task branch rather than discarding it
    // as a no-op. Safe: only reached when the task branch has no commits (so it's
    // not the checked-out branch), and it's a no-op when the agent left nothing.
    if !has_commits
        && worktree_head_ahead_of_base(&worktree.worktree_path, &worktree.base_branch)
            .unwrap_or(false)
        // Only adopt when HEAD is a proper continuation of base — never a diverged
        // or unrelated branch (council guard: ahead-count alone isn't ancestry).
        && base_is_ancestor_of_head(&worktree.worktree_path, &worktree.base_branch)
    {
        match squash_adopt_worktree_head_onto_branch(
            &worktree.worktree_path,
            &worktree.base_branch,
            &worktree.task_branch,
            &item.title,
        ) {
            Ok(_) => {
                warn!(
                    work_item_id = %item.work_item_id,
                    "work_item_dispatch: agent self-committed a clean tree — squashed/retitled onto the task branch instead of discarding as a no-op"
                );
                has_commits = true;
            }
            Err(e) => warn!(
                work_item_id = %item.work_item_id, error = %e,
                "work_item_dispatch: agent-committed HEAD detected but squash-onto-task-branch failed; treating as no-op"
            ),
        }
    }
    if !has_commits {
        // NO-OP IS NOT SUCCESS. A build work_item that produced no diff means the
        // required change was never applied — the backend stalled, refused, or
        // claimed "done" without editing (observed 2026-07-07: codex ran to a
        // clean exit on a moderate task but left the worktree clean, and the item
        // was silently marked `done` with no PR — a phantom success). Route it
        // through the failure-aware retry ladder instead: requeue with context so
        // the next attempt (or the cloud lane) can try, converging to terminal
        // `failed` after MAX_DISPATCH_ATTEMPTS so a genuinely-unbuildable task
        // surfaces to a human rather than masquerading as completed.
        warn!(
            work_item_id = %item.work_item_id,
            backend = %backend_used,
            agent_output_tail = %agent_output_tail,
            "work_item_dispatch: backend produced no diff (no commits) — treating as failed no-op, not done"
        );
        requeue_or_fail(
            &pg,
            &item,
            "backend produced no diff (no commits) — required change not applied",
        )
        .await?;
        remove_worktree(&item.repo_path, &worktree.worktree_path)?;
        return Ok(());
    }

    // SELF-VERIFY GATE — catch garbage at the source, before it costs a PR + CI
    // + review cycle. "Has a diff" != "is good": empty stub files or
    // non-compiling code must never reach the review drain (2026-07-20: an item
    // committed three 0-byte files and the review drain was the FIRST place it
    // was caught). On failure, give the same backend one repair turn in its warm
    // worktree, then re-run the complete gate before considering a PR.
    let mut self_fix_attempts = 0;
    loop {
        let Err(reason) =
            self_verify_worktree(&worktree.worktree_path, &worktree.base_branch).await
        else {
            break;
        };
        warn!(
            work_item_id = %item.work_item_id,
            %reason,
            self_fix_attempts,
            "work_item_dispatch: self-verify gate rejected the build before PR"
        );
        if self_fix_attempts >= MAX_SELF_FIX_ATTEMPTS {
            // Final escalation: the primary backend couldn't repair it. Hand the failure to the
            // idle 480B (the local "intense" tier) before giving up — instead of dying on the
            // exhausted cloud backstop. Reuses the Lane-1.5 480B permit so ring concurrency stays
            // bounded. This is the "route self-verify failures to the strongest local coder" fix.
            if let Some(_permit) = tokio::time::timeout(
                Duration::from_millis(LANE15_480B_PERMIT_WAIT_MS),
                crate::dispatch_concurrency::acquire_480b_permit(),
            )
            .await
            .ok()
            .and_then(Result::ok)
            {
                info!(
                    work_item_id = %item.work_item_id,
                    "work_item_dispatch: self-verify — escalating final repair to the 480B"
                );
                let fix_prompt = format!(
                    "A prior implementation failed the mandatory pre-PR verification gate:\n\n{reason}\n\nFix the failure in this repo now, scoped strictly to the original work item, and leave all edits uncommitted."
                );
                let _ = tokio::time::timeout(
                    Duration::from_secs(LANE1_TIMEOUT_SECS),
                    crate::codegen_apply::codegen_apply(
                        &pg,
                        &worktree.worktree_path,
                        &fix_prompt,
                        Some(LANE15_480B_MODEL_HINT),
                        2,
                    ),
                )
                .await;
                commit_worktree_changes(
                    &worktree.worktree_path,
                    &item.title,
                    &git_name,
                    &git_email,
                )?;
                if self_verify_worktree(&worktree.worktree_path, &worktree.base_branch)
                    .await
                    .is_ok()
                {
                    info!(
                        work_item_id = %item.work_item_id,
                        "work_item_dispatch: 480B self-verify rescue PASSED — proceeding to PR"
                    );
                    break;
                }
                warn!(
                    work_item_id = %item.work_item_id,
                    "work_item_dispatch: 480B self-verify rescue did not pass — failing"
                );
            }
            requeue_or_fail(
                &pg,
                &item,
                &format!("self-verify failed before opening PR after {self_fix_attempts} self-fix attempt(s) + 480B rescue: {reason}"),
            )
            .await?;
            remove_worktree(&item.repo_path, &worktree.worktree_path)?;
            return Ok(());
        }

        self_fix_attempts += 1;
        let prompt = format!(
            "Your implementation failed the mandatory pre-PR verification gate:\n\n{reason}\n\nFix the failure in this same worktree now. Keep the change scoped to the original work item, run the relevant verification yourself, and leave all edits uncommitted. Do not open a PR."
        );
        let fix = crate::cli_executor::execute_cli_in_dir(
            &backend_used,
            &prompt,
            &[],
            Some(&worktree.worktree_path),
            Some(Duration::from_secs(FF_TIMEOUT_SECS)),
        )
        .await;
        match fix {
            Ok(result) if result.exit_code == 0 => {}
            Ok(result) => {
                warn!(
                    work_item_id = %item.work_item_id,
                    backend = %backend_used,
                    exit_code = result.exit_code,
                    stderr = %err_tail(&result.stderr),
                    "work_item_dispatch: same-agent self-fix turn failed"
                );
            }
            Err(error) => warn!(
                work_item_id = %item.work_item_id,
                backend = %backend_used,
                error = format!("{error:#}"),
                "work_item_dispatch: same-agent self-fix turn could not run"
            ),
        }
        // Preserve and verify any repair diff even if the backend exited
        // non-zero after editing (commonly its own final test command failing).
        commit_worktree_changes(&worktree.worktree_path, &item.title, &git_name, &git_email)?;
    }

    let head_sha = git_head_sha(&worktree.worktree_path)?;
    push_branch(&item.repo_path, &worktree.task_branch)?;
    let pr_url = create_pr(&worktree.worktree_path, &item, &worktree).await?;
    record_pr_provenance(&pg, &item, &backend_used, &pr_url).await?;

    // In-place review (Pillar-4 v2): judge the change IN the still-warm build
    // workspace — diff vs base + item spec + a real `cargo test` run — before
    // it enters the merge queue. Review is fail-closed: the serial drain is a
    // pure merger and only sees rows carrying an approval from this folder.
    let review = match run_in_place_review(&pg, &item, &worktree, &backend_used).await {
        Ok(r) => r,
        Err(e) => {
            requeue_or_fail(&pg, &item, &format!("in-place review unavailable: {e:#}")).await?;
            remove_worktree(&item.repo_path, &worktree.worktree_path)?;
            return Ok(());
        }
    };
    if !review.approved {
        let r = &review;
        let reason = format!("in-place review rejected by {}: {}", r.reviewer, r.reason);
        warn!(
            work_item_id = %item.work_item_id,
            reviewer = %r.reviewer,
            builder = %backend_used,
            reason = %r.reason,
            "work_item_dispatch: in-place review REJECTED — failing item with reason"
        );
        // Best-effort: persist the verdict on the merge-queue row even though
        // the item never enqueues, so rejections feed v_reviewer_stats too.
        if let Err(e) =
            record_review_rejection(&pg, &item, &worktree, &head_sha, &pr_url, &backend_used, r)
                .await
        {
            warn!(
                work_item_id = %item.work_item_id, error = %e,
                "work_item_dispatch: failed to record review rejection on queue row (non-fatal)"
            );
        }
        // Same retry-with-context ladder as a build failure: the reviewer's
        // reason lands in last_error so the next attempt sees what to fix.
        requeue_or_fail(&pg, &item, &reason).await?;
        remove_worktree(&item.repo_path, &worktree.worktree_path)?;
        return Ok(());
    }

    mark_ready_for_review(
        &pg,
        &item,
        &worktree,
        &head_sha,
        &pr_url,
        &backend_used,
        Some(&review),
    )
    .await?;
    Ok(())
}

async fn record_pr_provenance(
    pg: &PgPool,
    item: &AssignedWorkItem,
    builder: &str,
    pr_url: &str,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO work_item_provenance
              (work_item_id, builder_model, builder_computer, builder_port, builder_lane,
               pr_url, pr_created_at, pr_created_by)
            SELECT $1, $2, $3,
                   NULLIF(substring(l.endpoint FROM ':(\d+)(?:/|$)'), '')::int,
                   CASE WHEN l.endpoint LIKE 'cloud:%' OR $2 ~ '^(codex|claude|kimi|gemini|grok)(:|$)'
                        THEN 'cloud' ELSE 'local' END,
                   $4, NOW(), $5
              FROM work_item_leases l WHERE l.work_item_id = $1
              ORDER BY l.created_at DESC LIMIT 1
            ON CONFLICT (work_item_id) DO UPDATE SET
              builder_model = EXCLUDED.builder_model,
              builder_computer = EXCLUDED.builder_computer,
              builder_port = EXCLUDED.builder_port,
              builder_lane = EXCLUDED.builder_lane,
              pr_url = EXCLUDED.pr_url,
              pr_created_at = COALESCE(work_item_provenance.pr_created_at, EXCLUDED.pr_created_at),
              pr_created_by = EXCLUDED.pr_created_by,
              updated_at = NOW()"#,
    )
    .bind(item.work_item_id)
    .bind(builder)
    .bind(&item.computer_name)
    .bind(pr_url)
    .bind(format!("sub-agent:{} / {builder}", item.sub_agent_id))
    .execute(pg)
    .await?;
    Ok(())
}

/// RAII guard that keeps a work_item lease's heartbeat fresh while alive and
/// signals the heartbeat task to stop on drop — i.e. when `dispatch_one` returns
/// on ANY path (success, no-commit early return, or error). This holds the lease
/// for the whole dispatch (build → commit → push → PR) so the leader watchdog
/// can't reap it mid-finalize.
pub(crate) struct HeartbeatGuard {
    stop_tx: watch::Sender<bool>,
}

impl HeartbeatGuard {
    pub(crate) fn spawn(work_item_id: Uuid) -> Self {
        let (stop_tx, stop_rx) = watch::channel(false);
        // Detached: the task loops on its own timer and exits promptly when the
        // guard's drop sends `true` on stop_tx. (spawn_heartbeat already
        // tokio::spawns; dropping the JoinHandle is the intentional detach.)
        drop(spawn_heartbeat(work_item_id, stop_rx));
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
            Ok(pool) => match ff_db::pg_heartbeat_work_item_lease(
                &pool,
                work_item_id,
                crate::work_item_scheduler::LEASE_GRANT_SECS,
            )
            .await
            {
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

/// Bump `dispatch_tick_at` for every active lease on this host. Best-effort:
/// if the update fails (e.g. the host has no computers row yet), the next tick
/// retries. A missed tick eventually triggers the stale-dispatch-tick reaper.
async fn bump_dispatch_tick_at(pg: &PgPool, worker_name: &str) {
    if let Err(e) = sqlx::query(
        "UPDATE work_item_leases
            SET dispatch_tick_at = NOW()
          WHERE computer_id = (SELECT id FROM computers WHERE name = $1)
            AND released_at IS NULL",
    )
    .bind(worker_name)
    .execute(pg)
    .await
    {
        warn!(
            worker_name = %worker_name,
            error = %e,
            "work_item_dispatch: failed to bump dispatch_tick_at"
        );
    }
}

/// Human-readable task branch: `feature/<title-slug>-<id4>` (operator
/// directive 2026-07-19 — "name them feature/<work item name> instead of the
/// id"). The 4-hex id tail keeps the name DETERMINISTIC per item (retries must
/// regenerate the identical branch for `checkout -B` + force-with-lease to
/// converge) and unique when two items share a title. Slug: lowercase
/// alphanumerics, runs of everything else collapsed to `-`, capped at 40 chars.
fn task_branch_name(title: &str, work_item_id: uuid::Uuid) -> String {
    let mut slug = String::with_capacity(40);
    let mut last_dash = true; // suppress a leading dash
    for c in title.chars() {
        if slug.len() >= 40 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-');
    let id4 = &work_item_id.simple().to_string()[..4];
    if slug.is_empty() {
        format!("feature/{id4}")
    } else {
        format!("feature/{slug}-{id4}")
    }
}

async fn create_worktree_for_item(pg: &PgPool, item: &AssignedWorkItem) -> Result<WorktreeRecord> {
    let base_branch = match item.base_branch.as_deref() {
        Some(branch) if !branch.trim().is_empty() => branch.trim().to_string(),
        _ => default_branch(&item.repo_path).unwrap_or_else(|_| "main".to_string()),
    };
    let task_branch = task_branch_name(&item.title, item.work_item_id);

    // Canonical slots reuse an existing project checkout (~/projects/{project});
    // don't create a synthetic sub-agent-N directory for them.
    if item.kind != "canonical" {
        let _ = ensure_workspaces((item.slot.max(0) as u32) + 1)
            .map_err(|e| anyhow!("ensure sub-agent workspaces: {e}"))?;
    }

    // NO git worktrees (operator decision 2026-07-17): every build runs
    // directly in the slot's own clone — fetch + full clean + `checkout -B`
    // from origin/<base>. `-B` resets any leftover task branch, so retries are
    // collision-free with zero worktree bookkeeping. Repo resolution
    // guarantees repo_path is the slot clone whenever the item has a repo_url;
    // a repo-url-less item bound outside the slot uses that path the same way
    // (it is the only workspace that exists for it).
    let worktree_path = item.repo_path.clone();
    insert_worktree_creating(pg, item, &worktree_path, &base_branch, &task_branch).await?;
    match checkout_clone_for_build(&item.repo_path, &base_branch, &task_branch) {
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

/// Atomically CLAIM the item for THIS dispatch: flip `claimed` → `building`, but
/// ONLY if it's still `claimed`. Returns `false` when 0 rows change — another
/// concurrent dispatch already claimed it (dispatches now run spawned/concurrent,
/// #72), so the caller skips instead of double-dispatching. The `WHERE
/// status = 'claimed'` guard is the compare-and-set (council guard): "probably
/// before the next tick" is not a concurrency guarantee, the DB predicate is. On
/// a win, also flip the lease to `building` and refresh its heartbeat.
async fn mark_building(pg: &PgPool, item: &AssignedWorkItem) -> Result<bool> {
    let mut tx = pg.begin().await?;
    let claimed = sqlx::query(
        "UPDATE work_items SET status = 'building' WHERE id = $1 AND status = 'claimed'",
    )
    .bind(item.work_item_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        // Lost the race (already building/in_review/done) — don't touch the lease.
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE work_item_leases
            SET lease_state = 'building', heartbeat_at = NOW(),
                lease_expires_at = GREATEST(lease_expires_at, NOW() + make_interval(secs => $3))
          WHERE work_item_id = $1
            AND sub_agent_id = $2
            AND released_at IS NULL",
    )
    .bind(item.work_item_id)
    .bind(item.sub_agent_id)
    .bind(crate::work_item_scheduler::LEASE_GRANT_SECS as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

async fn mark_ready_for_review(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
    head_sha: &str,
    pr_url: &str,
    builder: &str,
    review: Option<&ReviewOutcome>,
) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query(
        "UPDATE work_items
            SET status = 'in_review',
                branch_name = $2,
                pr_url = $3,
                cleanup_complete = TRUE
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
            (work_item_id, project_id, status, branch_name, pr_url, head_sha,
             builder, reviewer, review_verdict, review_reason,
             review_started_at, review_completed_at, reviewer_computer)
        VALUES ($1, $2, 'queued', $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (work_item_id) DO UPDATE
            SET status = 'queued',
                branch_name = EXCLUDED.branch_name,
                pr_url = EXCLUDED.pr_url,
                head_sha = EXCLUDED.head_sha,
                failed_at = NULL,
                failure_reason = NULL,
                builder = EXCLUDED.builder,
                reviewer = EXCLUDED.reviewer,
                review_verdict = EXCLUDED.review_verdict,
                review_reason = EXCLUDED.review_reason,
                review_started_at = EXCLUDED.review_started_at,
                review_completed_at = EXCLUDED.review_completed_at,
                reviewer_computer = EXCLUDED.reviewer_computer
        "#,
    )
    .bind(item.work_item_id)
    .bind(&item.project_id)
    .bind(&worktree.task_branch)
    .bind(pr_url)
    .bind(head_sha)
    .bind(builder)
    .bind(review.map(|r| r.reviewer.as_str()))
    .bind(review.map(|r| if r.approved { "approve" } else { "reject" }))
    .bind(review.map(|r| truncate_for_db(&r.reason)))
    .bind(review.map(|r| r.started_at))
    .bind(review.map(|r| r.completed_at))
    .bind(review.map(|_| item.computer_name.as_str()))
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"INSERT INTO work_item_provenance
              (work_item_id, builder_model, builder_computer, builder_port, builder_lane,
               reviewer_model, reviewer_computer, reviewer_port, reviewer_lane,
               pr_url, pr_created_at, pr_created_by, updated_at)
            SELECT $1, $2, $3,
                   NULLIF(substring(l.endpoint FROM ':(\d+)(?:/|$)'), '')::int,
                   CASE WHEN l.endpoint LIKE 'cloud:%' OR $2 ~ '^(codex|claude|kimi|gemini|grok)(:|$)'
                        THEN 'cloud' ELSE 'local' END,
                   $4, $7, $8,
                   CASE WHEN $4 LIKE 'local:%' THEN 'local' ELSE 'cloud' END,
                   $5, NOW(), $6, NOW()
              FROM work_item_leases l WHERE l.work_item_id = $1
              ORDER BY l.created_at DESC LIMIT 1
            ON CONFLICT (work_item_id) DO UPDATE SET
              builder_model = EXCLUDED.builder_model,
              builder_computer = EXCLUDED.builder_computer,
              builder_port = EXCLUDED.builder_port,
              builder_lane = EXCLUDED.builder_lane,
              reviewer_model = EXCLUDED.reviewer_model,
              reviewer_computer = EXCLUDED.reviewer_computer,
              reviewer_port = EXCLUDED.reviewer_port,
              reviewer_lane = EXCLUDED.reviewer_lane,
              pr_url = EXCLUDED.pr_url,
              pr_created_at = COALESCE(work_item_provenance.pr_created_at, EXCLUDED.pr_created_at),
              pr_created_by = EXCLUDED.pr_created_by,
              updated_at = NOW()"#,
    )
    .bind(item.work_item_id)
    .bind(builder)
    .bind(&item.computer_name)
    .bind(review.map(|r| r.reviewer.as_str()))
    .bind(pr_url)
    .bind(format!("sub-agent:{} / {builder}", item.sub_agent_id))
    .bind(
        review
            .and_then(|r| r.reviewer_computer.as_deref())
            .unwrap_or(&item.computer_name),
    )
    .bind(review.and_then(|r| r.reviewer_port))
    .execute(&mut *tx)
    .await?;

    // Folder ownership spans build -> review -> merge. Keep the slot occupied
    // until the serial drain reports the merged signal; the host reaper then
    // deletes the branch/tree before this folder can claim another item.
    sqlx::query(
        "UPDATE work_item_leases SET lease_state = 'reviewing', heartbeat_at = NOW() \
          WHERE work_item_id = $1 AND sub_agent_id = $2 AND released_at IS NULL",
    )
    .bind(item.work_item_id)
    .bind(item.sub_agent_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

// ── Pillar 4 v2: in-place review stage (after build+PR, before enqueue) ──────

/// Reviewer label recorded when the qwen3-coder-480b ring reviews an item.
const LOCAL_REVIEWER_480B: &str = "local:qwen3-coder-480b";

/// Hard cap on a single cloud reviewer invocation.
const REVIEW_CLOUD_TIMEOUT: Duration = Duration::from_secs(600);

/// Hard cap on a 480B ring review (matches the merge drain's budget).
const REVIEW_480B_TIMEOUT: Duration = Duration::from_secs(300);

/// Budget for the `cargo test` run whose output is fed to the reviewer. The
/// per-slot CARGO_TARGET_DIR is warm from the build, so this is incremental.
const REVIEW_CARGO_TEST_TIMEOUT: Duration = Duration::from_secs(900);

/// Global concurrency gate for the qwen3-coder-480b ring: 2 permits (what the
/// single ring instance sustains). BUILDS (Lane-1.5) have PRIORITY on the 480B
/// — it is the only local escalation tier, so a build path may block on
/// `acquire()`. REVIEW may only ever `try_acquire`: a free permit means the
/// 480B reviews for $0, an unavailable permit means the review falls straight
/// to a cloud reviewer WITHOUT waiting (review has 3 cloud alternatives;
/// builds have none locally).
pub(crate) static GATE_480B: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(2));

/// Outcome of one in-place review: who judged it, the verdict + rationale, and
/// the start/complete timestamps written to the merge-queue row (the latency
/// signal that feeds `v_reviewer_stats` and future routing weights).
struct ReviewOutcome {
    reviewer: String,
    reviewer_computer: Option<String>,
    reviewer_port: Option<i32>,
    approved: bool,
    reason: String,
    started_at: chrono::DateTime<chrono::Utc>,
    completed_at: chrono::DateTime<chrono::Utc>,
}

/// A `local` build may have escalated to the 480B inside the codegen cascade,
/// so any local builder conservatively excludes the 480B reviewer (rule 1:
/// never the model that built the change). Pure for testability.
fn builder_excludes_480b(builder: &str) -> bool {
    let b = builder.to_ascii_lowercase();
    b == "local" || b.starts_with("local:") || b.contains("480b")
}

fn same_model_family(builder: &str, reviewer: &str) -> bool {
    let builder = builder.to_ascii_lowercase();
    let reviewer = reviewer.to_ascii_lowercase();
    builder == reviewer
        || builder.ends_with(&format!(":{reviewer}"))
        || builder.starts_with(&format!("{reviewer}:"))
}

/// Observed per-reviewer review history (recent window), read from the same
/// merge-queue columns `v_reviewer_stats` aggregates.
#[derive(Debug, Clone)]
struct ReviewerStat {
    reviewer: String,
    reviews: i64,
    avg_latency_secs: f64,
}

async fn cloud_reviewer_stats(pg: &PgPool) -> Result<Vec<ReviewerStat>> {
    let rows = sqlx::query(
        "SELECT reviewer, COUNT(*) AS reviews, \
                AVG(EXTRACT(EPOCH FROM (review_completed_at - review_started_at)))::float8 \
                    AS avg_latency_secs \
           FROM work_item_merge_queue \
          WHERE reviewer IS NOT NULL \
            AND review_started_at IS NOT NULL \
            AND review_completed_at IS NOT NULL \
            AND review_completed_at > NOW() - INTERVAL '7 days' \
          GROUP BY reviewer",
    )
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ReviewerStat {
            reviewer: r.get("reviewer"),
            reviews: r.get("reviews"),
            avg_latency_secs: r.try_get::<f64, _>("avg_latency_secs").unwrap_or(0.0),
        })
        .collect())
}

/// Assumed latency for a reviewer with no observed history. Only used as the
/// weighting unit; an unknown reviewer's score is 0 (tried first) regardless.
const DEFAULT_REVIEW_LATENCY_SECS: f64 = 180.0;

/// Latency-weighted round-robin order for the cloud trio (rule 2), builder
/// excluded (rule 1). Score = observed review count × observed avg latency, so
/// a reviewer twice as fast earns twice the turns before its score catches up,
/// and a reviewer with no history scores 0 — tried first so the fleet gathers
/// latency data on every backend. Pure for testability.
fn order_cloud_reviewers(
    builder: &str,
    stats: &[ReviewerStat],
    backends: &[String],
) -> Vec<String> {
    let mut scored: Vec<(f64, String)> = backends
        .iter()
        .filter(|b| !same_model_family(builder, b))
        .map(|b| {
            let score = stats
                .iter()
                .find(|s| s.reviewer.eq_ignore_ascii_case(b))
                .map(|s| {
                    let lat = if s.avg_latency_secs > 0.0 {
                        s.avg_latency_secs
                    } else {
                        DEFAULT_REVIEW_LATENCY_SECS
                    };
                    s.reviews as f64 * lat
                })
                .unwrap_or(0.0);
            (score, b.to_string())
        })
        .collect();
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, b)| b).collect()
}

/// Run the in-place review in the warm build workspace. Reviewer selection:
///   1. NEVER the model that built the change (builder recorded alongside).
///   2. Cloud trio (claude/codex/kimi) in latency-weighted round-robin order,
///      falling through to the next on a backend failure.
///   3. The 480B ring participates only when [`GATE_480B`] has a free permit
///      RIGHT NOW (`try_acquire`) — builds have priority on the ring, and a
///      busy ring means cloud review without waiting.
/// `Err` means NO reviewer produced a verdict — the caller enqueues unreviewed
/// (fail-open) rather than stranding an already-pushed PR.
async fn run_in_place_review(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
    builder: &str,
) -> Result<ReviewOutcome> {
    let prompt = build_review_prompt(item, worktree).await;

    if !builder_excludes_480b(builder) {
        if let Ok(permit) = GATE_480B.try_acquire() {
            let started_at = chrono::Utc::now();
            let verdict = review_via_480b_inplace(pg, &prompt).await;
            drop(permit);
            match verdict {
                Ok((approved, reason, reviewer_computer, reviewer_port)) => {
                    return Ok(ReviewOutcome {
                        reviewer: LOCAL_REVIEWER_480B.to_string(),
                        reviewer_computer: Some(reviewer_computer),
                        reviewer_port,
                        approved,
                        reason,
                        started_at,
                        completed_at: chrono::Utc::now(),
                    });
                }
                Err(e) => warn!(
                    work_item_id = %item.work_item_id,
                    error = format!("{e:#}"),
                    "work_item_dispatch: 480b in-place review unavailable — falling to cloud"
                ),
            }
        } else {
            info!(
                work_item_id = %item.work_item_id,
                "work_item_dispatch: 480b busy (builds have priority) — cloud review without waiting"
            );
        }
    }

    let stats = cloud_reviewer_stats(pg).await.unwrap_or_default();
    // The fleet inventory is the routing source of truth. This deliberately
    // avoids a compiled-in provider list/model hint and follows newly enrolled
    // authenticated cloud CLIs without a daemon rebuild.
    let backends: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT backend FROM computer_backends \
          WHERE installed AND authenticated ORDER BY backend",
    )
    .fetch_all(pg)
    .await
    .context("load authenticated review backends")?;
    let mut last_err: Option<anyhow::Error> = None;
    for backend in order_cloud_reviewers(builder, &stats, &backends) {
        let started_at = chrono::Utc::now();
        match crate::cli_executor::execute_cli_in_dir(
            &backend,
            &prompt,
            &[],
            Some(&worktree.worktree_path),
            Some(REVIEW_CLOUD_TIMEOUT),
        )
        .await
        {
            Ok(res) if res.exit_code == 0 && !res.stdout.trim().is_empty() => {
                record_review_interaction(
                    pg,
                    &backend,
                    &prompt,
                    &res.stdout,
                    i32::try_from(res.duration_ms).ok(),
                )
                .await;
                let (approved, reason) =
                    crate::work_item_merge_drain::parse_review_response(&res.stdout);
                return Ok(ReviewOutcome {
                    reviewer: backend,
                    reviewer_computer: Some(item.computer_name.clone()),
                    reviewer_port: None,
                    approved,
                    reason,
                    started_at,
                    completed_at: chrono::Utc::now(),
                });
            }
            Ok(res) => {
                let e = anyhow!(
                    "{backend} exited {}: {}",
                    res.exit_code,
                    err_tail(&res.stderr)
                );
                warn!(backend = %backend, error = %e, "work_item_dispatch: in-place review backend failed — trying next");
                last_err = Some(e);
            }
            Err(e) => {
                warn!(backend = %backend, error = format!("{e:#}"), "work_item_dispatch: in-place review backend unavailable — trying next");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no in-place review backend available")))
}

/// One 480B ring review. Caller holds a [`GATE_480B`] permit. `Err` means the
/// ring didn't serve the call (routing failed, timed out, or `fleet_oneshot`
/// failed over to a weaker model — never trusted as a 480B verdict).
async fn review_via_480b_inplace(
    pg: &PgPool,
    prompt: &str,
) -> Result<(bool, String, String, Option<i32>)> {
    let resp =
        crate::fleet_oneshot::fleet_oneshot(pg, prompt, Some("480b"), Some(REVIEW_480B_TIMEOUT))
            .await
            .context("480b in-place review")?;
    if !crate::work_item_merge_drain::served_by_480b(&resp.model) {
        bail!(
            "480b ring unavailable — fleet_oneshot failed over to {} on {}",
            resp.model,
            resp.worker_name
        );
    }
    record_review_interaction(
        pg,
        &resp.model,
        prompt,
        &resp.text,
        i32::try_from(resp.latency_ms).ok(),
    )
    .await;
    let (approved, reason) = crate::work_item_merge_drain::parse_review_response(&resp.text);
    let port = resp
        .endpoint
        .rsplit_once(':')
        .and_then(|(_, value)| value.trim_end_matches('/').parse().ok());
    Ok((approved, reason, resp.worker_name, port))
}

/// Reviewer input: the branch diff vs base + the item spec + a real
/// `cargo test` run from the warm workspace. Never fails — a missing piece is
/// reported inline so the reviewer knows what it could not see.
async fn build_review_prompt(item: &AssignedWorkItem, worktree: &WorktreeRecord) -> String {
    let base = &worktree.base_branch;
    let diff = run_git(
        &worktree.worktree_path,
        ["diff", &format!("origin/{base}...HEAD")],
        Duration::from_secs(60),
    )
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    .unwrap_or_else(|| "(diff unavailable)".to_string());
    let diff: String = diff.chars().take(40_000).collect();
    let test_output = run_workspace_cargo_test(&worktree.worktree_path).await;

    format!(
        "You are reviewing a change built by an autonomous coding fleet, in the build workspace, \
         before it enters the merge queue.\n\
         Judge whether the diff correctly and cleanly implements the requested work item.\n\n\
         Work item title:\n{title}\n\n\
         Work item description:\n{description}\n\n\
         Requirements for approval:\n\
         - The diff matches the stated intent.\n\
         - The diff introduces no regressions.\n\
         - The diff does NOT DEGRADE existing code, documentation, comments, tests, or behavior.\n\
         - The diff is a real, complete change rather than a placeholder, superficial edit, or \
           partial implementation.\n\
         - The `cargo test` output below does not show failures caused by this change.\n\n\
         Answer with exactly APPROVE or REJECT on the first line. Put a one-line reason on the \
         next line.\n\n\
         Branch diff vs origin/{base} (truncated to 40000 chars if needed):\n\
         ```diff\n{diff}\n```\n\n\
         `cargo test` output from the build workspace:\n```\n{test_output}\n```",
        title = item.title,
        description = item.description.as_deref().unwrap_or_default(),
    )
}

/// `cargo test` in the warm build workspace so the reviewer sees real test
/// results, not a guess. Reuses the per-slot CARGO_TARGET_DIR (incremental —
/// the build already compiled here). Any failure to run is reported as text.
async fn run_workspace_cargo_test(worktree_path: &Path) -> String {
    if !worktree_path.join("Cargo.toml").exists() {
        return "(not a cargo workspace — no test run)".to_string();
    }
    let cwd = worktree_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("cargo");
        cmd.arg("test")
            .current_dir(&cwd)
            .env("CARGO_TARGET_DIR", slot_cargo_target(&cwd));
        match run_command_capture(cmd, REVIEW_CARGO_TEST_TIMEOUT) {
            Ok(out) => format!("(exit: {})\n{}", out.status, agent_output_tail(&out, 6000)),
            Err(e) => format!("cargo test did not complete: {e:#}"),
        }
    })
    .await
    .unwrap_or_else(|e| format!("cargo test task failed: {e}"))
}

/// Best-effort `ff_interactions` row for one in-place review turn (training
/// data — the point of routing review through ff). Never fails the dispatch.
async fn record_review_interaction(
    pg: &PgPool,
    engine: &str,
    prompt: &str,
    response: &str,
    latency_ms: Option<i32>,
) {
    let rec = ff_db::InteractionRecord {
        channel: "dispatch_inplace_review".to_string(),
        request_text: prompt.chars().take(16000).collect(),
        engine: Some(engine.to_string()),
        response_text: response.chars().take(16000).collect(),
        latency_ms,
        outcome: "success".to_string(),
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(pg, &rec).await {
        warn!(error = %e, "work_item_dispatch: failed to log review interaction (non-fatal)");
    }
}

/// Persist a REJECT verdict on the merge-queue row (status 'failed') so
/// rejections feed `v_reviewer_stats` even though the item never enqueues. A
/// later successful retry's enqueue overwrites this row via its upsert.
async fn record_review_rejection(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
    head_sha: &str,
    pr_url: &str,
    builder: &str,
    review: &ReviewOutcome,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO work_item_merge_queue
            (work_item_id, project_id, status, branch_name, pr_url, head_sha,
             failed_at, failure_reason,
             builder, reviewer, review_verdict, review_reason,
             review_started_at, review_completed_at, reviewer_computer)
        VALUES ($1, $2, 'failed', $3, $4, $5, NOW(), $6, $7, $8, 'reject', $9, $10, $11, $12)
        ON CONFLICT (work_item_id) DO UPDATE
            SET status = 'failed',
                branch_name = EXCLUDED.branch_name,
                pr_url = EXCLUDED.pr_url,
                head_sha = EXCLUDED.head_sha,
                failed_at = NOW(),
                failure_reason = EXCLUDED.failure_reason,
                builder = EXCLUDED.builder,
                reviewer = EXCLUDED.reviewer,
                review_verdict = 'reject',
                review_reason = EXCLUDED.review_reason,
                review_started_at = EXCLUDED.review_started_at,
                review_completed_at = EXCLUDED.review_completed_at,
                reviewer_computer = EXCLUDED.reviewer_computer
        "#,
    )
    .bind(item.work_item_id)
    .bind(&item.project_id)
    .bind(&worktree.task_branch)
    .bind(pr_url)
    .bind(head_sha)
    .bind(truncate_for_db(&format!(
        "in-place review rejected by {}: {}",
        review.reviewer, review.reason
    )))
    .bind(builder)
    .bind(&review.reviewer)
    .bind(truncate_for_db(&review.reason))
    .bind(review.started_at)
    .bind(review.completed_at)
    .bind(&item.computer_name)
    .execute(pg)
    .await?;
    sqlx::query(
        "UPDATE work_item_provenance SET reviewer_model = $2, reviewer_computer = $3, \
                reviewer_port = $4, reviewer_lane = CASE WHEN $2 LIKE 'local:%' \
                THEN 'local' ELSE 'cloud' END, updated_at = NOW() WHERE work_item_id = $1",
    )
    .bind(item.work_item_id)
    .bind(&review.reviewer)
    .bind(
        review
            .reviewer_computer
            .as_deref()
            .unwrap_or(&item.computer_name),
    )
    .bind(review.reviewer_port)
    .execute(pg)
    .await?;
    Ok(())
}

/// Max dispatch attempts before a work_item is escalated to terminal `failed`
/// instead of being requeued. Each retry re-runs the dispatch with the prior
/// error appended to the item's `last_error` so the next attempt has context.
const MAX_DISPATCH_ATTEMPTS: i32 = 3;

/// Escalation ladder: after this many prior attempts, SKIP the local codegen lane
/// and go straight to the cloud/CLI backstop, which is more capable. Below this,
/// try cheap-local-first.
///
/// Set to 1 (was 2) because the LOCAL lane starves the dispatch heartbeat and gets
/// reaped at ~190s (#62 — codegen_apply blocks the async runtime), so a SECOND
/// local attempt just burns another ~190s + slot cycle for a task that keeps
/// hanging the same way, before finally reaching cloud. The CLOUD lane does NOT
/// starve the heartbeat (verified: cloud attempts run 900s+ with no stale-reap).
/// Mechanical and moderate tasks use the local lane; complex or multi-file-heavy
/// tasks already `prefers_cloud_lane`, so this narrows the local exposure to ONE
/// cheap try then the reliable cloud lane — unblocking the backlog with no
/// heartbeat rearchitecture.
/// Whether to try the cheap LOCAL codegen lane for this dispatch: only while UNDER
/// the cloud-escalation threshold, the node's local-codegen breaker is closed, and
/// the task isn't complexity-routed to cloud. Pure so the routing is testable —
/// the `ESCALATE_TO_CLOUD_AT = 1` value means a mechanical task gets ONE local try
/// then goes cloud (#62: the local lane starves the heartbeat; cloud does not).
fn use_local_lane(attempts: i32, breaker_open: bool, prefers_cloud: bool) -> bool {
    let requirements = ff_routing_policy::TaskRequirements {
        prior_failure_count: attempts.max(0) as u32,
        capability_tags: if prefers_cloud {
            vec!["cloud".to_string()]
        } else {
            Vec::new()
        },
        ..Default::default()
    };
    ff_routing_policy::use_local_30b(&requirements, &ff_routing_policy::PolicyConfig::default())
        && !breaker_open
}

/// Hard ceiling on the Lane-1 LOCAL codegen harness — kept STRICTLY BELOW the
/// lease heartbeat-staleness window (`LEASE_STALE_SECS`) so a slow/hung local
/// lane always fails over to the cloud backstop BEFORE the scheduler's
/// stale-lease reaper can reclaim the lease.
///
/// Why sub-stale (observed 2026-07-08 on beyonce/DGX): the local fleet-model
/// codegen takes MINUTES per round and often emits invalid SEARCH/REPLACE blocks
/// ("SEARCH block not found"). With the old 7-min ceiling, a run that starved the
/// dispatch's own heartbeat (the local model blocking the runtime) was reaped at
/// `LEASE_STALE_SECS` (180s) and KILLED mid-flight — so the `lane1_failed` breaker
/// signal below never recorded, the local-codegen breaker never opened to skip the
/// wasteful lane, and the task re-queued and starved again (→ "3 stalled attempts"
/// → failed). A sub-stale ceiling makes the local lane lose the race to the cloud
/// backstop (codex, ~8s here), not to the reaper: it self-aborts, records the
/// breaker failure, and codex lands the change within the same lease.
const LANE1_TIMEOUT_SECS: u64 =
    crate::work_item_scheduler::LEASE_STALE_SECS as u64 - LANE1_STALE_MARGIN_SECS;
/// Margin kept between the Lane-1 ceiling and the lease-staleness window so the
/// `tokio::time::timeout` fires + cleanup + breaker-record all complete before the
/// reaper's next tick.
const LANE1_STALE_MARGIN_SECS: u64 = 30;
// Compile-time invariant: Lane-1 must self-abort strictly before the reaper.
const _: () = assert!(LANE1_TIMEOUT_SECS < crate::work_item_scheduler::LEASE_STALE_SECS as u64);

const LANE15_480B_MODEL_HINT: &str = "local:qwen3-coder-480b";
const LANE15_480B_PROVIDER: &str = "local_480b";
const LANE15_480B_PERMITS: usize = crate::dispatch_concurrency::MAX_480B_CONCURRENCY;
const LANE15_480B_PERMIT_WAIT_MS: u64 = 750;

async fn lane15_enabled(pg: &PgPool) -> bool {
    matches!(
        ff_db::pg_read_gate_value(pg, "work_item_lane15_mode", "on", "on")
            .await
            .as_deref(),
        Ok("on") | Ok("true") | Ok("1")
    )
}

fn should_attempt_lane15(
    lane1_failed: bool,
    complexity_at_least_moderate: bool,
    enabled: bool,
    breaker_open: bool,
) -> bool {
    (lane1_failed || complexity_at_least_moderate) && enabled && !breaker_open
}

/// Whether the task's decomposer-assigned complexity (`mechanical` |
/// `moderate` | `complex`) is high enough to warrant the Lane-1.5 480B
/// escalation BEFORE the cloud backstop, even when Lane 1 was never attempted
/// (e.g. `moderate`/`complex` tasks that already skip Lane 1 via
/// `prefers_cloud_lane`). Mechanical tasks only reach Lane 1.5 on an actual
/// Lane-1 failure.
fn complexity_at_least_moderate(complexity: &str) -> bool {
    matches!(complexity, "moderate" | "complex")
}

#[cfg(test)]
async fn try_acquire_lane15_480b_permit(
    sem: &Semaphore,
    wait: Duration,
) -> Option<SemaphorePermit<'_>> {
    tokio::time::timeout(wait, sem.acquire())
        .await
        .ok()
        .and_then(Result::ok)
}

#[cfg(test)]
async fn acquire_within(sem: &Semaphore, wait: Duration) -> Option<SemaphorePermit<'_>> {
    try_acquire_lane15_480b_permit(sem, wait).await
}

async fn mark_lease_endpoint(pg: &PgPool, item: &AssignedWorkItem, endpoint: &str) {
    if let Err(e) = sqlx::query(
        "UPDATE work_item_leases
            SET endpoint = $3
          WHERE work_item_id = $1
            AND sub_agent_id = $2
            AND released_at IS NULL",
    )
    .bind(item.work_item_id)
    .bind(item.sub_agent_id)
    .bind(endpoint)
    .execute(pg)
    .await
    {
        warn!(
            work_item_id = %item.work_item_id,
            endpoint,
            error = %e,
            "work_item_dispatch: failed to mark lease endpoint"
        );
    }
}

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

/// A clean, quick exit with no stdout and no diff is a backend failure: the
/// task never received a usable agent result. Timeouts without a diff are the
/// same class regardless of elapsed time.
fn backend_failed_without_output(
    timed_out: bool,
    status_ok: bool,
    stdout: &[u8],
    elapsed: Duration,
    worktree_has_diff: bool,
) -> bool {
    !worktree_has_diff
        && (timed_out
            || (status_ok
                && stdout.iter().all(u8::is_ascii_whitespace)
                && elapsed < Duration::from_secs(30)))
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

/// A quick clean exit without any stdout is a broken CLI invocation, not a
/// task-level no-op. Slow successful runs may legitimately be quiet until the
/// end, so only the observed sub-30-second failure signature is special-cased.
fn quick_empty_success_is_provider_failure(output: &Output, elapsed: Duration) -> bool {
    output.status.success()
        && output.stdout.iter().all(u8::is_ascii_whitespace)
        && elapsed <= Duration::from_secs(30)
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
          WHERE id = $1
            AND status NOT IN ('done', 'merged', 'cancelled')",
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
          WHERE id = $1
            AND status NOT IN ('done', 'merged', 'cancelled')",
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
    notify_operator_task_failed(pg, item, error).await;
    Ok(())
}

/// Best-effort Telegram notification when a work_item exhausts its retry budget
/// and lands on terminal `failed` — "Jarvis tells you when it's genuinely
/// stuck." Reads the same `telegram_bot_token` / `telegram_chat_id` secrets
/// the alert evaluator uses. NEVER returns an error or panics: any failure is logged and swallowed.
async fn notify_operator_task_failed(pg: &PgPool, item: &AssignedWorkItem, error: &str) {
    let work_item_id = item.work_item_id;
    let token = match ff_db::pg_get_secret(pg, "telegram_bot_token").await {
        Ok(Some(t)) if !t.trim().is_empty() => t,
        _ => {
            tracing::debug!("notify_operator_task_failed: no telegram bot token; skipping");
            return;
        }
    };
    let chat_id = match ff_db::pg_get_secret(pg, "telegram_chat_id").await {
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
    let text = task_failed_alert_text(item, error);
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

fn task_failed_alert_text(item: &AssignedWorkItem, error: &str) -> String {
    let err_clip: String = error.chars().take(800).collect();
    let session_name = format!("sub-agent-{}", item.slot);
    let session_id = item
        .session_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "not recorded".to_string());

    format!(
        "🛑 {title}\n\nFAILED after max retries\nForgeFleet couldn't complete this task after all retry attempts.\n\nSession: {session_name} on {computer_name}\n\nLast error (diagnostic):\n{err_clip}\n\nIDs (diagnostic)\nwork_item: {work_item_id}\nsession: {session_id}\ncomputer: {computer_id}",
        title = item.title,
        computer_name = item.computer_name,
        work_item_id = item.work_item_id,
        computer_id = item.computer_id,
    )
}

async fn release_slot_and_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    item: &AssignedWorkItem,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        "WITH releasing AS (
             SELECT id, work_item_id, lease_state, endpoint, attempt, computer_id
               FROM work_item_leases
              WHERE work_item_id = $1
                AND sub_agent_id = $2
                AND released_at IS NULL
              FOR UPDATE
         ), released AS (
             UPDATE work_item_leases l
                SET lease_state = 'released',
                    released_at = NOW(),
                    release_reason = $3
               FROM releasing r
              WHERE l.id = r.id
          RETURNING r.work_item_id,
                    r.lease_state AS from_status,
                    r.endpoint,
                    r.attempt,
                    l.release_reason,
                    r.computer_id
         )
         INSERT INTO work_item_events
             (work_item_id, from_status, to_status, computer, attempt, detail)
         SELECT r.work_item_id,
                r.from_status,
                'lease_released',
                c.name,
                r.attempt,
                NULLIF(concat_ws('/', NULLIF(r.endpoint, ''), CASE
                    WHEN NULLIF(r.endpoint, '') IS NULL THEN NULL
                    WHEN r.endpoint LIKE 'cloud:%'
                      OR r.endpoint ~ '^(codex|claude|kimi|gemini|grok)(:|$)'
                      THEN 'cloud'
                    ELSE 'local'
                END), '')
           FROM released r
           LEFT JOIN computers c ON c.id = r.computer_id",
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
targeted test pass, STOP. LEAVE YOUR EDITS UNCOMMITTED in the working tree — do NOT \
run `git add`, `git commit`, `git push`, or open a PR. NEVER commit your edits yourself: \
the harness can only commit and push UNCOMMITTED working-tree changes. If you run git \
commit the tree becomes CLEAN and the harness DISCARDS your finished work as a no-op \
(your task then fails despite the code being correct).\n\
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
    let phase = |name: &str, steps: &[String]| {
        if steps.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n{name}:\n{}",
                steps
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    };
    let context = if item.context.as_object().is_some_and(|v| !v.is_empty()) {
        format!(
            "\n\nAttached context (treat triggering evidence as authoritative):\n{}",
            item.context
        )
    } else {
        String::new()
    };
    format!(
        "Target repo:\n- project_id: {}\n- repo_url: {}\n- checkout: {}\n\n{}{}{}{}{}{}{}",
        item.project_id,
        item.repo_url.as_deref().unwrap_or("unknown"),
        item.repo_path.display(),
        task,
        context,
        phase("PRE_WORK — complete before editing", &item.pre_work),
        phase("WORK — execute in order", &item.work),
        phase("POST_WORK — complete after implementation", &item.post_work),
        retry_context,
        DISPATCH_HOUSE_RULES,
    )
}

/// Parse the token count a vendor CLI reports in its output so the training
/// corpus captures token economics, not just content. codex prints
/// `tokens used\n9,332`; kimi/others print variants like `Tokens: 1234` or
/// `total tokens: 1234`. Best-effort — returns 0 when no count is found.
#[doc(hidden)]
pub fn parse_cli_tokens(output: &str) -> i32 {
    crate::llm_attribution::parse_total_tokens_marker(output)
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
    let request_text = dispatch_prompt(item);
    let (response_text, outcome, error_text, tokens_in, tokens_out, tokens_estimated) = match result
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout)
                .chars()
                .take(16000)
                .collect::<String>();
            // Scan stdout AND stderr — several vendor CLIs print their usage
            // stats on stderr — then degrade to a flagged chars/4 estimate so
            // the usage rollup counts this call instead of recording 0/0.
            let combined = format!("{text}\n{}", String::from_utf8_lossy(&out.stderr));
            let (tin, tout) = crate::llm_attribution::parse_cli_token_counts(&combined);
            let (tin, tout, estimated) =
                crate::llm_attribution::tokens_or_estimate(tin, tout, &request_text, &text);
            (text, "success".to_string(), None, tin, tout, estimated)
        }
        Err(e) => (
            String::new(),
            "error".to_string(),
            // Full anyhow chain ({:#}) so the real cause is captured, not just
            // the top-level wrapper (e.g. "fleet_oneshot round 1").
            Some(format!("{e:#}").chars().take(2000).collect::<String>()),
            0,
            0,
            false,
        ),
    };

    // Track the recurrence count for this error signature and populate the
    // interaction-log column that the leader's self-heal tick aggregates.
    let error_signature = error_text.as_deref().map(|err| {
        let tracker = crate::log_signature::global_tracker();
        let sig = tracker.signature_for(err);
        // Also update the process-level tracker so recurrence counts are
        // available for diagnostics even though the DB is the canonical store.
        tracker.observe(err);
        sig
    });

    // Vendor backends keep their name (claude/codex/kimi); the local codegen
    // lane's "local" backend stays `local`. Cost comes from the config-driven
    // rates table — $0 for local, published per-token rates for cloud.
    let engine = crate::llm_attribution::engine_label(backend);
    let cost_usd = crate::llm_attribution::cost_usd(&engine, tokens_in, tokens_out);
    let rec = ff_db::InteractionRecord {
        channel: "work_item_dispatch".to_string(),
        request_text,
        request_meta: serde_json::json!({ "tokens_estimated": tokens_estimated }),
        engine: Some(engine),
        response_text,
        tokens_in,
        tokens_out,
        cost_usd,
        latency_ms: i32::try_from(elapsed.as_millis()).ok(),
        outcome,
        error_text,
        error_signature,
        worker_name: Some(worker_name.to_string()),
        endpoint: Some(format!("ff cli {backend}")),
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(pg, &rec).await {
        warn!(error = %e, "work_item_dispatch: failed to log interaction (non-fatal)");
    }
}

/// Lane 1.5 route: acquire a permit on the shared process-wide 480B semaphore
/// and run one bounded round on the local 480B codegen endpoint. Mirrors the
/// Lane-1 dispatch pattern (bounded timeout, breaker bookkeeping, synthetic
/// Output on success) but gated on the shared ring's concurrency instead of
/// Lane 1's per-node breaker. Returns `Some((backend, output))` when the
/// change lands; `None` when the caller should pass through to the Lane 2
/// cloud backstop (ring busy, no-op, error, or timeout).
async fn dispatch_to_480b(
    pg: &PgPool,
    item: &AssignedWorkItem,
    worktree: &WorktreeRecord,
    prompt: &str,
) -> Option<(String, Output)> {
    let _permit = match tokio::time::timeout(
        Duration::from_millis(LANE15_480B_PERMIT_WAIT_MS),
        crate::dispatch_concurrency::acquire_480b_permit(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    {
        Some(permit) => permit,
        None => {
            info!(
                work_item_id = %item.work_item_id,
                stage = "lane1.5",
                permits = LANE15_480B_PERMITS,
                wait_ms = LANE15_480B_PERMIT_WAIT_MS,
                "work_item_dispatch: Lane-1.5 480B ring busy; skipping to cloud backstop"
            );
            return None;
        }
    };

    mark_lease_endpoint(pg, item, "lane1.5:local:qwen3-coder-480b").await;
    info!(
        work_item_id = %item.work_item_id,
        stage = "lane1.5",
        provider = LANE15_480B_PROVIDER,
        "work_item_dispatch: starting Lane-1.5 480B codegen"
    );
    let lane15 = tokio::time::timeout(
        Duration::from_secs(LANE1_TIMEOUT_SECS),
        crate::codegen_apply::codegen_apply(
            pg,
            &worktree.worktree_path,
            prompt,
            Some(LANE15_480B_MODEL_HINT),
            1,
        ),
    )
    .await;
    match lane15 {
        Ok(Ok(outcome)) if outcome.applied => {
            let _ = crate::circuit_breaker::record_provider_success(
                pg,
                item.computer_id,
                LANE15_480B_PROVIDER,
            )
            .await;
            info!(
                work_item_id = %item.work_item_id,
                stage = "lane1.5",
                rounds = outcome.rounds,
                "work_item_dispatch: Lane-1.5 480B codegen landed the change"
            );
            Some((
                "local-480b".to_string(),
                synthetic_output(&outcome.final_diff.unwrap_or_else(|| "applied".into())),
            ))
        }
        Ok(Ok(outcome)) => {
            let _ = crate::circuit_breaker::record_provider_failure(
                pg,
                item.computer_id,
                LANE15_480B_PROVIDER,
                "local_codegen_unavailable",
            )
            .await;
            info!(
                work_item_id = %item.work_item_id,
                stage = "lane1.5",
                error = ?outcome.error,
                "work_item_dispatch: Lane-1.5 480B codegen didn't land; backstop to cloud"
            );
            None
        }
        Ok(Err(e)) => {
            let _ = crate::circuit_breaker::record_provider_failure(
                pg,
                item.computer_id,
                LANE15_480B_PROVIDER,
                "local_codegen_unavailable",
            )
            .await;
            warn!(
                work_item_id = %item.work_item_id,
                stage = "lane1.5",
                error = format!("{e:#}"),
                "work_item_dispatch: Lane-1.5 480B codegen errored; backstop to cloud"
            );
            None
        }
        Err(_) => {
            let _ = crate::circuit_breaker::record_provider_failure(
                pg,
                item.computer_id,
                LANE15_480B_PROVIDER,
                "timeout",
            )
            .await;
            warn!(
                work_item_id = %item.work_item_id,
                stage = "lane1.5",
                timeout_secs = LANE1_TIMEOUT_SECS,
                "work_item_dispatch: Lane-1.5 480B codegen TIMED OUT; backstop to cloud"
            );
            None
        }
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
    let mut prompt = dispatch_prompt(item);
    // Prepend a Cortex context pack: the exact existing symbols this task touches,
    // pulled from the shared code graph, so the agent starts there instead of
    // grep-storming the whole repo cold (wasted context + the cold-compile explore
    // phase). Prefer the precomputed context stored on the work_item row; fall back
    // to a live `ff cortex find` lookup only when nothing is stored. Fail-open.
    let pack = crate::dispatch_context::context_pack_for_dispatch(
        item.brain_node_ids.clone(),
        item.touched_paths.clone(),
        item.title.clone(),
        item.description.clone().unwrap_or_default(),
        worktree.worktree_path.clone(),
        8,
    )
    .await;
    if !pack.is_empty() {
        info!(work_item_id = %item.work_item_id, pack_bytes = pack.len(), "run_ff_dispatch: prepended Cortex context pack");
        prompt = format!("{pack}\n{prompt}");
    }

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
    let lane15_enabled = lane15_enabled(pg).await;
    let lane15_breaker_open =
        crate::circuit_breaker::is_provider_open(pg, item.computer_id, LANE15_480B_PROVIDER)
            .await
            .unwrap_or(false);

    // Lane 1: local codegen harness — but skip it once we've escalated to cloud
    // (stage 2, after ESCALATE_TO_CLOUD_AT local failures it has failed the same
    // way), when this node's local-codegen breaker is open (it's been failing),
    // OR when the task is complexity-routed to cloud (complex or multi-file-
    // heavy) — the local lane wedges/half-finishes on those, so we send them
    // straight to the capable cloud CLI from attempt 0 instead of burning a
    // wedge-prone local attempt first.
    let mut lane1_failed_or_timed_out = false;
    if use_local_lane(item.attempts, lane1_breaker_open, item.prefers_cloud_lane()) {
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
            Ok(Ok(outcome)) if outcome.already_done => {
                // The model inspected the repo and reports the task is ALREADY implemented
                // (feature exists, tests pass, nothing to commit). Mark the work_item done —
                // NOT failed — so an already-satisfied task drains instead of thrashing every
                // lane. Terminal 'done' is protected from later requeue_or_fail by its status guard.
                let _ = crate::circuit_breaker::record_provider_success(
                    pg,
                    item.computer_id,
                    LOCAL_CODEGEN_PROVIDER,
                )
                .await;
                info!(
                    work_item_id = %item.work_item_id,
                    "work_item_dispatch: task already implemented per model — marking work_item done"
                );
                let _ = sqlx::query(
                    "UPDATE work_items SET status = 'done', completed_at = NOW(), \
                        last_error = 'already implemented (model verified: feature exists + tests pass)' \
                      WHERE id = $1 AND status NOT IN ('merged')",
                )
                .bind(item.work_item_id)
                .execute(pg)
                .await;
                let _ = remove_worktree(&item.repo_path, &worktree.worktree_path);
                anyhow::bail!("already-implemented — work_item marked done, no PR needed");
            }
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
                lane1_failed_or_timed_out = true;
                lane1_failed("local_codegen_unavailable").await;
                info!(
                    work_item_id = %item.work_item_id,
                    error = ?outcome.error,
                    "work_item_dispatch: local codegen didn't land; trying Lane-1.5 before cloud backstop"
                );
            }
            Ok(Err(e)) => {
                lane1_failed_or_timed_out = true;
                lane1_failed("local_codegen_unavailable").await;
                warn!(
                    work_item_id = %item.work_item_id,
                    // Full anyhow chain so the REAL cause surfaces (e.g. the underlying
                    // fleet_oneshot failure), not just the "fleet_oneshot round 1" wrapper.
                    error = format!("{e:#}"),
                    "work_item_dispatch: local codegen errored; trying Lane-1.5 before cloud backstop"
                );
            }
            Err(_) => {
                lane1_failed_or_timed_out = true;
                lane1_failed("local_codegen_unavailable").await;
                warn!(
                    work_item_id = %item.work_item_id,
                    timeout_secs = LANE1_TIMEOUT_SECS,
                    "work_item_dispatch: local codegen TIMED OUT (hung) — trying Lane-1.5 before cloud backstop"
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

    // Lane 1.5: one bounded escalation round on the shared 480B coder ring. The
    // ring has one 11 tok/s instance with --parallel 2, so cap concurrency
    // process-wide and skip instead of queueing when both slots are busy.
    // Triggered either by a Lane-1 failure OR by the task's own complexity
    // (moderate/complex tasks that skip Lane 1 entirely via
    // `prefers_cloud_lane` still get one local-480b shot before cloud).
    let lane15_trigger =
        lane1_failed_or_timed_out || complexity_at_least_moderate(&item.complexity);
    if should_attempt_lane15(
        lane1_failed_or_timed_out,
        complexity_at_least_moderate(&item.complexity),
        lane15_enabled,
        lane15_breaker_open,
    ) {
        if let Some((backend, output)) = dispatch_to_480b(pg, item, worktree, &prompt).await {
            return Ok((backend, output));
        }
    } else if lane15_trigger && !lane15_enabled {
        info!(
            work_item_id = %item.work_item_id,
            stage = "lane1.5",
            "work_item_dispatch: Lane-1.5 disabled by work_item_lane15_mode; skipping to cloud backstop"
        );
    } else if lane15_trigger && lane15_breaker_open {
        info!(
            work_item_id = %item.work_item_id,
            stage = "lane1.5",
            "work_item_dispatch: local_480b breaker OPEN on this node; skipping Lane-1.5 to cloud backstop"
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
    // dying. Falls back to fast/reliable `claude` when no backend is known
    // dispatchable; loaded build nodes have shown codex needs materially longer.
    let routed = routed_backends(pg, item.computer_id, 5400).await;
    let mut backends = if routed.is_empty() {
        let otherwise_dispatchable = ff_db::pg_dispatchable_backends(pg, item.computer_id, 5400)
            .await
            .unwrap_or_default();
        // The router excluded every backend (headroom/freshness/rank filters), but authenticated
        // backends ARE dispatchable and cloud budget is open. Rather than bail "exhausted" (a false
        // signal that killed items while cloud sat at 60-66% weekly usage), FALL BACK to those
        // dispatchable backends. The per-backend cloud-error nervous system below still handles a
        // genuinely quota-blocked backend by failing over / classifying — so real exhaustion is
        // still caught at call time, not pre-emptively guessed here.
        if !otherwise_dispatchable.is_empty() {
            warn!(
                work_item_id = %item.work_item_id,
                backends = ?otherwise_dispatchable,
                "work_item_dispatch: router returned no backends; falling back to dispatchable cloud backends (was: bail exhausted)"
            );
            otherwise_dispatchable
        } else {
            vec!["claude".to_string()]
        }
    } else {
        routed
    };
    // Claude is the fast build backstop; loaded nodes can make codex exceed a
    // short probe even though it succeeds given time.
    let mut policy = ff_routing_policy::PolicyConfig::default();
    policy.preferred_cloud_backstop = Some("claude".to_string());
    ff_routing_policy::promote_cloud_backstop(&mut backends, &policy);
    let computer_id = item.computer_id;
    let forced_backend = primary_or_default_backend(&backends);
    let mut attempted_backend = false;
    let mut quota_skipped_backend = false;
    let mut last_output: Option<(String, Output)> = None;
    // Capture EVERY backend's error so a total failure surfaces WHY for ALL of
    // them (codex + claude + kimi) in the DB `last_error` — not just the last —
    // ending the SSH-into-node log-diving needed to see codex/claude when only
    // kimi (the last tried) was recorded. Each error is tail-trimmed since the
    // full command echoes the huge prompt; the status/stderr lives at the end.
    let mut backend_errors: Vec<String> = Vec::new();

    for backend in &backends {
        let budget = crate::cloud_budget::provider_budget(pg, backend).await;
        if crate::cloud_budget::is_exhausted(budget.as_ref(), chrono::Utc::now()) {
            quota_skipped_backend = true;
            info!(
                backend = %backend,
                exhausted_until = ?budget.as_ref().and_then(|row| row.window_exhausted_until),
                "run_ff_dispatch: skipping quota-exhausted backend"
            );
            continue;
        }
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
            let started = Instant::now();
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
                        crate::cloud_budget::record_success(
                            pg,
                            backend,
                            budget.as_ref().and_then(|row| row.window_exhausted_until),
                        )
                        .await;
                        return Ok((
                            backend.clone(),
                            synthetic_output("salvaged diff after backend timeout"),
                        ));
                    }
                    // The backend may have COMMITTED its changes and then hung.
                    // A clean tree is not "no work done" when HEAD advanced past base.
                    if head_has_deliverable_commits(&worktree.worktree_path, &worktree.base_branch)
                    {
                        warn!(backend = %backend, error = %e, "run_ff_dispatch: backend timed out but self-committed — salvaging");
                        let _ = crate::circuit_breaker::record_provider_success(
                            pg,
                            computer_id,
                            backend,
                        )
                        .await;
                        return Ok((
                            backend.clone(),
                            synthetic_output("salvaged self-commits after backend timeout"),
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
                    let _ = crate::cloud_budget::record_backend_failure(pg, backend).await;
                    break; // try the next routed backend
                }
            };
            if backend_failed_without_output(
                false,
                out.status.success(),
                &out.stdout,
                started.elapsed(),
                worktree_has_diff(&worktree.worktree_path),
            ) {
                warn!(backend = %backend, "run_ff_dispatch: backend exited cleanly with empty stdout and no diff — switching");
                backend_errors.push(format!(
                    "{backend}: clean exit with empty stdout and no diff"
                ));
                let _ = crate::circuit_breaker::record_provider_failure(
                    pg,
                    computer_id,
                    backend,
                    "empty_stdout",
                )
                .await;
                let _ = crate::cloud_budget::record_backend_failure(pg, backend).await;
                break;
            }
            if out.status.success() {
                let _ =
                    crate::circuit_breaker::record_provider_success(pg, computer_id, backend).await;
                crate::cloud_budget::record_success(
                    pg,
                    backend,
                    budget.as_ref().and_then(|row| row.window_exhausted_until),
                )
                .await;
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
            // A non-zero exit that STILL wrote a real diff (the backend edited
            // files, then its own final verify/exit step failed) is salvageable
            // work, not a discard — commit it and let CI judge, exactly like the
            // timeout-salvage path above. Without this, switching to run_command_capture
            // (so exit-3 no-ops surface) would silently drop these diffs.
            if worktree_has_diff(&worktree.worktree_path) {
                warn!(backend = %backend, code = ?out.status.code(), "run_ff_dispatch: backend exited non-zero but wrote a real diff — salvaging");
                let _ =
                    crate::circuit_breaker::record_provider_success(pg, computer_id, backend).await;
                crate::cloud_budget::record_success(
                    pg,
                    backend,
                    budget.as_ref().and_then(|row| row.window_exhausted_until),
                )
                .await;
                return Ok((
                    backend.clone(),
                    synthetic_output("salvaged diff after non-zero backend exit"),
                ));
            }
            // Same self-commit case: backend committed, then its own verify step
            // failed and exited non-zero. The commits are still deliverable.
            if head_has_deliverable_commits(&worktree.worktree_path, &worktree.base_branch) {
                warn!(backend = %backend, code = ?out.status.code(), "run_ff_dispatch: backend exited non-zero but self-committed — salvaging");
                let _ =
                    crate::circuit_breaker::record_provider_success(pg, computer_id, backend).await;
                return Ok((
                    backend.clone(),
                    synthetic_output("salvaged self-commits after non-zero backend exit"),
                ));
            }
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            let class = crate::cloud_error::classify(backend, out.status.code(), &combined);
            if let Some(window) = crate::cloud_budget::failure_window(&combined) {
                let _ = crate::cloud_budget::record_failure(pg, backend, window).await;
            }
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
        if quota_skipped_backend {
            bail!("run_ff_dispatch: no dispatchable backend on this node (cloud budget exhausted)");
        }
        warn!(
            backend = %forced_backend,
            "run_ff_dispatch: all routed backends were skipped before launch; forcing one direct attempt"
        );
        let started = std::time::Instant::now();
        match run_backend_cli(&forced_backend, &worktree.worktree_path, &prompt).await {
            Ok(out) => {
                if quick_empty_success_is_provider_failure(&out, started.elapsed()) {
                    let _ = crate::cloud_budget::record_failure(
                        pg,
                        &forced_backend,
                        Duration::from_secs(30 * 60),
                    )
                    .await;
                    bail!("forced backend {forced_backend} exited successfully with empty stdout");
                }
                if out.status.success() {
                    crate::cloud_budget::record_success(pg, &forced_backend, None).await;
                }
                return Ok((forced_backend, out));
            }
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
                if head_has_deliverable_commits(&worktree.worktree_path, &worktree.base_branch) {
                    warn!(backend = %forced_backend, error = %e, "run_ff_dispatch: forced backend timed out but self-committed — salvaging");
                    let _ = crate::circuit_breaker::record_provider_success(
                        pg,
                        computer_id,
                        &forced_backend,
                    )
                    .await;
                    return Ok((
                        forced_backend,
                        synthetic_output("salvaged self-commits after forced backend timeout"),
                    ));
                }
                let _ = crate::circuit_breaker::record_provider_failure(
                    pg,
                    computer_id,
                    &forced_backend,
                    "timeout",
                )
                .await;
                let _ = crate::cloud_budget::record_failure(
                    pg,
                    &forced_backend,
                    Duration::from_secs(30 * 60),
                )
                .await;
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
        .unwrap_or_else(|| "claude".to_string())
}

/// The backend name for interaction attribution when a dispatch errored before
/// any backend produced output (so `run_ff_dispatch` returned Err, carrying no
/// backend). Best-effort: the first routed backend, else the historical default.
async fn primary_dispatch_backend(pg: &PgPool, computer_id: Uuid) -> String {
    primary_or_default_backend(&routed_backends(pg, computer_id, 5400).await)
}

static CAPACITY_SNAPSHOT: std::sync::OnceLock<ff_capacity::CapacitySnapshot> =
    std::sync::OnceLock::new();
static CAPACITY_LOAD_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Read cloud routing inputs from the registry's in-process snapshot. Cold
/// start is deliberately non-blocking: one background load is started and this
/// request uses the legacy SQL picker. Refresh failures retain the last good
/// snapshot inside `ff-capacity`, so registry outages do not enter the token
/// path.
async fn routed_backends(pg: &PgPool, computer_id: Uuid, fresh_secs: i64) -> Vec<String> {
    if let Some(snapshot) = CAPACITY_SNAPSHOT.get() {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(fresh_secs);
        let budgets = snapshot
            .cloud_budgets()
            .into_iter()
            .map(ff_routing_policy::CloudBudget::from)
            .collect::<Vec<_>>();
        let decision = evaluate_cached_backends(
            snapshot.backend_capacity(computer_id),
            &budgets,
            cutoff,
            "dispatch",
        );
        if let Err(error) = ff_db::pg_record_route_decision(pg, &decision).await {
            tracing::warn!(%error, trace_id = %decision.trace_id, "work_item_dispatch: route decision capture failed");
        }
        let selected = decision
            .candidates
            .iter()
            .filter(|candidate| !candidate.rejected)
            .map(|candidate| candidate.backend.clone())
            .collect::<Vec<_>>();
        tracing::info!(
            route_decision = ?serde_json::json!({
                "source": "capacity_snapshot",
                "snapshot_loaded_at": snapshot.loaded_at(),
                "computer_id": computer_id,
                "candidates": selected,
            }),
            "work_item_dispatch: backend route decision"
        );
        return selected;
    }

    if CAPACITY_LOAD_STARTED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_ok()
    {
        let pool = pg.clone();
        tokio::spawn(async move {
            match ff_capacity::CapacitySnapshot::load(&pool).await {
                Ok(snapshot) => {
                    let _ = CAPACITY_SNAPSHOT.set(snapshot);
                }
                Err(error) => {
                    CAPACITY_LOAD_STARTED.store(false, std::sync::atomic::Ordering::Release);
                    tracing::warn!(
                        %error,
                        "work_item_dispatch: capacity snapshot unavailable; legacy picker remains active"
                    );
                }
            }
        });
    }

    let selected = ff_db::pg_routed_backends(pg, computer_id, fresh_secs)
        .await
        .unwrap_or_default();
    tracing::info!(
        route_decision = ?serde_json::json!({
            "source": "legacy_pg_fallback",
            "computer_id": computer_id,
            "candidates": selected,
        }),
        "work_item_dispatch: backend route decision"
    );
    selected
}

fn evaluate_cached_backends(
    rows: Vec<ff_capacity::BackendCapacity>,
    budgets: &[ff_routing_policy::CloudBudget],
    cutoff: chrono::DateTime<chrono::Utc>,
    mode: &str,
) -> ff_routing_policy::RouteDecision {
    ff_routing_policy::evaluate_cloud_route(
        rows,
        budgets,
        cutoff,
        &ff_routing_policy::TaskRequirements::default(),
        &ff_routing_policy::PolicyConfig::default(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4(),
        mode,
    )
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
            // Exit 3 (not 0) if the CLI exits 0 but changes nothing, so run_ff_dispatch
            // can DISTINGUISH a real no-op from a silent stdin-pipe failure. A genuine
            // no-op (the change already exists on main) is treated as "already done" →
            // completed-without-PR, NOT a failure to requeue-to-death.
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
        // capture (not _timeout): keep the Output for ANY exit so run_ff_dispatch
        // can distinguish exit-3 (require-change no-op = already done) from a real
        // failure. Only spawn/timeout become Err here.
        run_command_capture(cmd, Duration::from_secs(FF_TIMEOUT_SECS + 30))
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

/// Environment variable holding an optional LAN git mirror URL PREFIX in
/// `insteadOf` style, e.g. `https://git-mirror.local/` or `git@git-mirror.local:`
/// — NOT a complete repo URL. The fetch URL is the prefix plus the GitHub
/// `owner/repo` path (same convention as `ha::mirror_service::fetch_mirror_sha`).
/// When set, fetches are routed through the mirror while pushes continue to
/// target the canonical GitHub origin (`git remote set-url --push`).
const LAN_MIRROR_URL_ENV: &str = "FORGEFLEET_LAN_MIRROR_URL";

/// Max attempts for each fetch phase (mirror and direct fallback).
const FETCH_ATTEMPTS: usize = 3;

/// Bounded exponential backoff base: 500ms → 1s → 2s before each retry.
const FETCH_BACKOFF_BASE_MS: u64 = 500;

/// Return a small non-cryptographic jitter (0..max_ms) derived from the current
/// time so concurrent retries across slots don't stampede the same remote.
fn fetch_jitter(max_ms: u64) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let jitter_ms = nanos % max_ms.max(1);
    Duration::from_millis(jitter_ms)
}

/// Build the full mirror repo URL for a slot's repo: the configured mirror
/// PREFIX plus the `owner/repo` path derived from the canonical GitHub URL
/// (which may use an ssh host alias like `git@github.com-venkat:owner/repo`).
/// Returns `None` when owner/repo cannot be derived — the caller then skips
/// the mirror and fetches directly from GitHub.
fn mirror_repo_url(mirror_prefix: &str, github_url: &str) -> Option<String> {
    let (owner, repo) = crate::project_github_sync::parse_owner_repo(github_url)?;
    Some(format!("{mirror_prefix}{owner}/{repo}"))
}

/// Prepare the slot's clone for a fresh build attempt. Fetch origin/<base>
/// (same no-stale-base contract as ever: fail rather than branch from a stale
/// ref), then a FULL clean — `reset --hard` + `clean -fd` — so leftovers from
/// a dead prior attempt can never leak into this build, then
/// `checkout -B wi/<id> origin/<base>`. `-B` resets the task branch even if a
/// prior attempt left it behind, which is what makes retries collision-free
/// without any worktree bookkeeping.
///
/// LAN mirror: if `FORGEFLEET_LAN_MIRROR_URL` is set, origin's fetch URL is
/// pointed at the mirror prefix + this repo's `owner/repo` path and `--push`
/// is pointed at the canonical GitHub URL.
/// Mirror fetches are retried with exponential backoff + jitter; if they all
/// fail we transparently fall back to a direct GitHub fetch.
fn checkout_clone_for_build(repo_path: &Path, base_branch: &str, task_branch: &str) -> Result<()> {
    let base_ref = format!("origin/{base_branch}");

    // Remember the canonical GitHub origin so we can restore it on fallback
    // and configure --push correctly when a mirror is in play. Read the PUSH
    // URL: it always stays on GitHub (only the fetch URL is ever pointed at
    // the mirror, and git falls back to the fetch URL when no push URL is
    // set), so this recovers the canonical URL even when a prior run died
    // with origin's fetch URL still pointing at the mirror.
    let github_url = run_git(
        repo_path,
        ["remote", "get-url", "--push", "origin"],
        Duration::from_secs(30),
    )
    .ok()
    .and_then(|o| {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    });

    // Optionally configure the LAN mirror: fetch from mirror, push to GitHub.
    // The env value is a URL PREFIX shared by every repo the mirror serves;
    // the per-repo fetch URL appends this repo's `owner/repo` path.
    let mirror_url = std::env::var(LAN_MIRROR_URL_ENV)
        .ok()
        .filter(|prefix| !prefix.trim().is_empty())
        .and_then(|prefix| match &github_url {
            Some(github) => {
                let url = mirror_repo_url(&prefix, github);
                if url.is_none() {
                    warn!(
                        mirror_prefix = %prefix,
                        github_url = %github,
                        "checkout_clone_for_build: cannot derive owner/repo for LAN mirror; fetching directly from GitHub"
                    );
                }
                url
            }
            None => None,
        });
    let mirror_configured = match (&mirror_url, &github_url) {
        (Some(mirror), Some(github)) if mirror != github => {
            run_git(
                repo_path,
                ["remote", "set-url", "origin", mirror],
                Duration::from_secs(30),
            )
            .with_context(|| format!("set origin fetch URL to LAN mirror {mirror}"))?;
            run_git(
                repo_path,
                ["remote", "set-url", "--push", "origin", github],
                Duration::from_secs(30),
            )
            .with_context(|| format!("set origin push URL to GitHub {github}"))?;
            info!(
                mirror_url = mirror,
                push_url = github,
                "checkout_clone_for_build: configured LAN mirror fetch with GitHub push"
            );
            true
        }
        _ => false,
    };

    let mut fetched = false;

    // Phase 1: fetch from the LAN mirror (if configured) with backoff + jitter.
    if mirror_configured {
        for attempt in 0..FETCH_ATTEMPTS {
            if attempt > 0 {
                let backoff =
                    Duration::from_millis(FETCH_BACKOFF_BASE_MS * (1u64 << (attempt - 1)));
                std::thread::sleep(backoff + fetch_jitter(200));
            }
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
                    warn!(
                        base_branch,
                        attempt,
                        error = %e,
                        "checkout_clone_for_build: LAN mirror fetch failed; retrying"
                    )
                }
            }
        }
    }

    // Phase 2: fallback to direct GitHub fetch, restoring the canonical origin.
    if !fetched {
        if mirror_configured {
            if let Some(github) = &github_url {
                run_git(
                    repo_path,
                    ["remote", "set-url", "origin", github],
                    Duration::from_secs(30),
                )
                .with_context(|| format!("restore origin URL to GitHub {github}"))?;
                // Push should also use GitHub now that we're not mirroring.
                let _ = run_git(
                    repo_path,
                    ["remote", "set-url", "--push", "origin", github],
                    Duration::from_secs(30),
                );
            }
            warn!(
                base_branch,
                "checkout_clone_for_build: LAN mirror fetch failed; falling back to direct GitHub fetch"
            );
        }
        for attempt in 0..FETCH_ATTEMPTS {
            if attempt > 0 {
                let backoff =
                    Duration::from_millis(FETCH_BACKOFF_BASE_MS * (1u64 << (attempt - 1)));
                std::thread::sleep(backoff + fetch_jitter(200));
            }
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
                    warn!(
                        base_branch,
                        attempt,
                        error = %e,
                        "checkout_clone_for_build: direct fetch failed; retrying"
                    )
                }
            }
        }
    }

    if !fetched {
        bail!(
            "checkout_clone_for_build: could not fetch origin/{base_branch} in {FETCH_ATTEMPTS} tries — \
             refusing to build from a possibly-stale base"
        );
    }
    run_git(repo_path, ["reset", "--hard"], Duration::from_secs(120))?;
    // `tmp/` is the slot's harvest boundary: a killed agent may have useful,
    // uncommitted output there even when git has no diff yet.
    run_git(
        repo_path,
        ["clean", "-fd", "-e", "tmp/"],
        Duration::from_secs(120),
    )?;
    run_git(
        repo_path,
        [
            OsStr::new("checkout"),
            OsStr::new("-B"),
            OsStr::new(task_branch),
            OsStr::new(&base_ref),
        ],
        Duration::from_secs(120),
    )?;
    Ok(())
}

fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    // Stage-2 clone-direct workspace: the "worktree" IS the clone. Never
    // remove the directory — park HEAD on detached origin HEAD and full-clean
    // so the next dispatch starts pristine. (`checkout -B` on the next attempt
    // resets the task branch, so leaving it behind here is harmless.)
    if worktree_path == repo_path {
        let _ = run_git(
            repo_path,
            ["checkout", "--detach", "HEAD"],
            Duration::from_secs(60),
        );
        let _ = run_git(repo_path, ["reset", "--hard"], Duration::from_secs(120));
        let _ = run_git(
            repo_path,
            ["clean", "-fd", "-e", "tmp/"],
            Duration::from_secs(120),
        );
        return Ok(());
    }
    // Legacy row from the pre-clone-direct era: the detached worktree dirs
    // were bulk-deleted fleet-wide on 2026-07-17, so all that's left is to
    // drop any straggler dir and let git forget the registration.
    if worktree_path.exists() {
        let _ = std::fs::remove_dir_all(worktree_path);
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
/// Last `max_chars` characters of an agent CLI's combined stdout+stderr, for
/// logging a no-diff outcome so it is diagnosable from the daemon log without a
/// host repro (#69: codex ran minutes on a DGX host and produced nothing — froze?
/// errored? claimed done without editing? — invisible in the summary-only log).
/// Char-safe (never splits a multi-byte boundary); a leading `…` marks truncation.
/// PURE + testable.
fn agent_output_tail(output: &std::process::Output, max_chars: usize) -> String {
    let combined = format!(
        "[stdout]\n{}\n[stderr]\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let total = combined.chars().count();
    if total <= max_chars {
        return combined;
    }
    let tail: String = combined.chars().skip(total - max_chars).collect();
    format!("…{tail}")
}

fn commit_worktree_changes(
    worktree_path: &Path,
    title: &str,
    author_name: &str,
    author_email: &str,
) -> Result<bool> {
    // Auto-format BEFORE staging so a fleet-produced Rust PR passes CI
    // `cargo fmt --check` — LLM backends routinely emit un-formatted Rust, which
    // fails the fmt gate and blocks the PR (observed on PR #787). Best-effort +
    // Rust-only: skip quietly when there's no Cargo.toml, and treat any fmt error
    // as non-fatal (commit as-is) so a missing toolchain / non-Rust repo never
    // breaks dispatch — it just falls back to the prior behavior.
    if worktree_path.join("Cargo.toml").exists() {
        if let Err(e) = run_cargo_fmt(worktree_path) {
            warn!(error = %e, "commit_worktree_changes: cargo fmt failed (best-effort) — committing as-is");
        }
    }
    run_git(worktree_path, ["add", "-A"], Duration::from_secs(60))?;
    let status = run_git(
        worktree_path,
        ["status", "--porcelain"],
        Duration::from_secs(30),
    )?;
    if status_output_is_clean(&status) {
        return Ok(false); // nothing to commit
    }
    let msg = format!("{title}\n\nAutomated work_item dispatch (ForgeFleet Pillar 4).");
    // Author with the resolved per-project identity via `-c` so it WINS over any
    // repo-local config a backend set during its run (codex writes
    // user.email=codex@openai.com into the slot clone; other agents leave
    // noreply). `-c` overrides both global and local config for this one
    // command, so fleet-authored commits always carry the operator/project
    // identity, not a bot. Identity is DB-driven (see resolve_git_identity).
    let name_cfg = format!("user.name={author_name}");
    let email_cfg = format!("user.email={author_email}");
    let commit = run_git(
        worktree_path,
        [
            OsStr::new("-c"),
            OsStr::new(&name_cfg),
            OsStr::new("-c"),
            OsStr::new(&email_cfg),
            OsStr::new("commit"),
            OsStr::new("-m"),
            OsStr::new(&msg),
        ],
        Duration::from_secs(60),
    );
    if let Err(commit_error) = commit {
        // A pre-commit hook may normalize generated files back to their checked-in
        // form. In that case `status` above was dirty, but `git commit` correctly
        // exits 1 with "nothing to commit" and leaves a clean tree. Treat that as
        // the same no-op as an initially-clean tree; otherwise the dispatcher
        // reports a failed build and retries an item that produced no durable diff.
        let post_hook_status = run_git(
            worktree_path,
            ["status", "--porcelain"],
            Duration::from_secs(30),
        )?;
        if status_output_is_clean(&post_hook_status) {
            warn!(
                error = %commit_error,
                "commit_worktree_changes: hook removed the staged diff; treating clean tree as no-op"
            );
            return Ok(false);
        }
        return Err(commit_error);
    }
    Ok(true)
}

/// Resolve a project's git author identity, DB-driven and per-project:
/// `projects.metadata` git_author_name/email first (so different projects can
/// commit under different identities), then the fleet-wide
/// `fleet_secrets` git.author_name/email default, then a final hardcoded
/// fallback so a commit never fails for lack of an identity.
async fn resolve_git_identity(pg: &PgPool, project_id: &str) -> (String, String) {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT metadata->>'git_author_name', metadata->>'git_author_email' \
           FROM projects WHERE id = $1",
    )
    .bind(project_id)
    .fetch_optional(pg)
    .await
    .ok()
    .flatten();
    let (mut name, mut email) = row.unwrap_or((None, None));
    if name.is_none() {
        name = crate::fleet_info::fetch_secret("git.author_name").await;
    }
    if email.is_none() {
        email = crate::fleet_info::fetch_secret("git.author_email").await;
    }
    (
        name.unwrap_or_else(|| "Venkat Yarlagadda".to_string()),
        email.unwrap_or_else(|| "venkat.yarl@gmail.com".to_string()),
    )
}

fn status_output_is_clean(status: &Output) -> bool {
    String::from_utf8_lossy(&status.stdout).trim().is_empty()
}

/// Run `cargo fmt` over the worktree so committed Rust is CI-fmt-clean. Uses a
/// login-ish shell that sources `~/.cargo/env` first (the daemon's own PATH may
/// omit `~/.cargo/bin` — the same reason the deploy playbook sources it), tries
/// the CI-pinned toolchain, then falls back to the default. The base tree is
/// already fmt-clean (it's origin/main), so this only reformats the agent's own
/// additions. Bounded; errors bubble up for the best-effort caller to log.
fn run_cargo_fmt(worktree_path: &Path) -> Result<()> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(". \"$HOME/.cargo/env\" 2>/dev/null || true; cargo +1.88.0 fmt 2>/dev/null || cargo fmt")
        .current_dir(worktree_path);
    run_command_timeout(cmd, Duration::from_secs(120))?;
    Ok(())
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

/// Whether the worktree's HEAD has commits ahead of `origin/<base>` — i.e. the
/// agent COMMITTED work (possibly on a detached / self-made HEAD) even though the
/// task branch shows none. Lets the no-diff path SALVAGE that work (#71/#69).
/// Falls back to the local base ref when `origin/<base>` isn't present.
fn worktree_head_ahead_of_base(worktree_path: &Path, base_branch: &str) -> Result<bool> {
    let range = format!("origin/{base_branch}..HEAD");
    let out = run_git(
        worktree_path,
        [
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new(&range),
        ],
        Duration::from_secs(30),
    )
    .or_else(|_| {
        let range = format!("{base_branch}..HEAD");
        run_git(
            worktree_path,
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

/// Whether `origin/<base>` (or the local base ref) is an ANCESTOR of the worktree
/// HEAD — i.e. HEAD is a proper CONTINUATION of base, not a diverged / unrelated
/// branch the agent happened to check out. Guards the salvage adoption so
/// `git branch -f` can never publish unrelated history (#71 council guard): a
/// positive `origin/base..HEAD` count alone only proves HEAD has commits base
/// lacks, NOT that HEAD descends from base. Uses the EXIT CODE of
/// `git merge-base --is-ancestor` (0=ancestor, 1=not); fails CLOSED (no salvage)
/// on any spawn/other error.
fn base_is_ancestor_of_head(worktree_path: &Path, base_branch: &str) -> bool {
    let run = |base_ref: String| {
        Command::new("git")
            .current_dir(worktree_path)
            .args(["merge-base", "--is-ancestor", &base_ref, "HEAD"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .and_then(|s| s.code())
    };
    match run(format!("origin/{base_branch}")) {
        Some(0) => true,
        Some(1) => false,
        // origin/<base> missing or errored — fall back to the local base ref.
        _ => matches!(run(base_branch.to_string()), Some(0)),
    }
}

/// Whether the worktree's HEAD has commits ahead of base AND those commits are a
/// proper continuation of base (not a diverged/unrelated branch). This is the
/// condition for treating the agent's self-made commits as the deliverable.
fn head_has_deliverable_commits(worktree_path: &Path, base_branch: &str) -> bool {
    worktree_head_ahead_of_base(worktree_path, base_branch).unwrap_or(false)
        && base_is_ancestor_of_head(worktree_path, base_branch)
}

/// Resolve the ref to use as the task branch's squash base. Prefers
/// `origin/<base_branch>` because the harness always fetches it; falls back to
/// the local base branch ref.
fn resolve_base_ref(worktree_path: &Path, base_branch: &str) -> Result<String> {
    let origin_ref = format!("origin/{base_branch}");
    if run_git(
        worktree_path,
        ["rev-parse", "--verify", &origin_ref],
        Duration::from_secs(30),
    )
    .is_ok()
    {
        return Ok(origin_ref);
    }
    if run_git(
        worktree_path,
        ["rev-parse", "--verify", base_branch],
        Duration::from_secs(30),
    )
    .is_ok()
    {
        return Ok(base_branch.to_string());
    }
    bail!("no base ref found for {base_branch} (needed to retitle self-commits)");
}

/// Adopt an agent's self-made HEAD commits onto the task branch the harness
/// pushes, then squash/retitle them into a single commit with the work-item
/// title (#71 / self-commit-salvage). Only called when `task_branch` has no
/// commits of its own, so it's safe to force-update it. Returns the new HEAD
/// SHA so the caller can avoid a second `git rev-parse`.
fn squash_adopt_worktree_head_onto_branch(
    worktree_path: &Path,
    base_branch: &str,
    task_branch: &str,
    title: &str,
) -> Result<String> {
    let base_ref = resolve_base_ref(worktree_path, base_branch)?;
    // Point the task branch at the agent's self-made HEAD.
    run_git(
        worktree_path,
        [
            OsStr::new("branch"),
            OsStr::new("-f"),
            OsStr::new(task_branch),
            OsStr::new("HEAD"),
        ],
        Duration::from_secs(30),
    )?;
    // Switch to it so the squash commit updates the branch we actually push.
    run_git(
        worktree_path,
        ["checkout", task_branch],
        Duration::from_secs(30),
    )?;
    // Squash everything since base into one commit with the canonical title.
    run_git(
        worktree_path,
        ["reset", "--soft", &base_ref],
        Duration::from_secs(30),
    )?;
    // Auto-format the agent's changes before recommitting, just like
    // commit_worktree_changes does for dirty trees. Best-effort + Rust-only.
    if worktree_path.join("Cargo.toml").exists() {
        if let Err(e) = run_cargo_fmt(worktree_path) {
            warn!(
                error = %e,
                "squash_adopt_worktree_head_onto_branch: cargo fmt failed (best-effort) — committing as-is"
            );
        }
    }
    // Stage any untracked files the agent left behind so they are folded into the
    // single commit instead of remaining dirty (matches commit_worktree_changes).
    let _ = run_git(worktree_path, ["add", "-A"], Duration::from_secs(30));
    let msg = format!("{title}\n\nAutomated work_item dispatch (ForgeFleet Pillar 4).");
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
    git_head_sha(worktree_path)
}

fn git_head_sha(worktree_path: &Path) -> Result<String> {
    let out = run_git(
        worktree_path,
        ["rev-parse", "HEAD"],
        Duration::from_secs(30),
    )?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Self-verify a built worktree BEFORE it becomes a PR — the cheap local checks
/// a competent engineer runs before calling a change "done": no empty /
/// whitespace-only added files, the workspace still compiles, and cheap tests
/// for directly affected crates pass.
/// Returns `Err(reason)` on the first problem so the retry gets actionable
/// context. Catching garbage here saves a PR + CI + review cycle (2026-07-20:
/// an item committed three 0-byte files and reached the review drain).
async fn self_verify_worktree(
    worktree_path: &Path,
    base_branch: &str,
) -> std::result::Result<(), String> {
    // 1) Reject empty / whitespace-only ADDED files (instant, catches the
    //    empty-stub failure mode directly).
    let range = format!("{base_branch}...HEAD");
    let out = run_git(
        worktree_path,
        ["diff", "--diff-filter=A", "--name-only", &range],
        Duration::from_secs(30),
    )
    .map_err(|e| format!("git diff for self-verify failed: {e}"))?;
    for f in String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        let full = worktree_path.join(f);
        let empty = match std::fs::read(&full) {
            Ok(bytes) => bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace),
            Err(_) => false, // unreadable/deleted — not this gate's concern
        };
        if empty {
            return Err(format!("added file is empty or whitespace-only: {f}"));
        }
    }

    // 2) Compile check — mandatory and bounded. A tool error or timeout is a
    //    failed verification too: inability to prove the diff builds must not
    //    turn into a PR. Skipped only for a non-Rust repo.
    if !worktree_path.join("Cargo.toml").exists() {
        return Ok(());
    }
    run_verification_command(
        worktree_path,
        &["check", "--workspace"],
        "cargo check --workspace",
        Duration::from_secs(300),
    )
    .await?;

    // 3) Test directly affected workspace crates when that is cheap. Limit the
    //    fan-out so a broad mechanical change does not monopolize a worker; CI
    //    remains responsible for the full workspace test matrix.
    for manifest in affected_crate_manifests(worktree_path, base_branch)?
        .into_iter()
        .take(3)
    {
        let manifest_arg = manifest.to_string_lossy().into_owned();
        run_verification_command(
            worktree_path,
            &["test", "--manifest-path", &manifest_arg, "--lib", "--quiet"],
            &format!("cargo test --manifest-path {manifest_arg} --lib"),
            Duration::from_secs(300),
        )
        .await?;
    }
    Ok(())
}

async fn run_verification_command(
    worktree_path: &Path,
    args: &[&str],
    label: &str,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let output = tokio::time::timeout(
        timeout,
        tokio::process::Command::new("cargo")
            .args(args)
            .current_dir(worktree_path)
            .output(),
    )
    .await
    .map_err(|_| format!("{label} timed out after {}s", timeout.as_secs()))?
    .map_err(|e| format!("could not run {label}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(1200)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(format!("{label} failed: {}", tail.trim()));
    }
    Ok(())
}

fn affected_crate_manifests(
    worktree_path: &Path,
    base_branch: &str,
) -> std::result::Result<Vec<PathBuf>, String> {
    let range = format!("{base_branch}...HEAD");
    let out = run_git(
        worktree_path,
        ["diff", "--name-only", &range],
        Duration::from_secs(30),
    )
    .map_err(|e| format!("git diff for affected tests failed: {e}"))?;
    let mut manifests = Vec::new();
    for path in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = Path::new(path).components();
        if parts.next().is_some_and(|p| p.as_os_str() == "crates") {
            if let Some(crate_dir) = parts.next() {
                let relative = PathBuf::from("crates").join(crate_dir).join("Cargo.toml");
                if worktree_path.join(&relative).is_file() && !manifests.contains(&relative) {
                    manifests.push(relative);
                }
            }
        }
    }
    Ok(manifests)
}

fn push_branch(repo_path: &Path, task_branch: &str) -> Result<()> {
    // Prune-fetch first: the lease below is checked against this clone's
    // remote-tracking ref, and merged wi/* branches are DELETED on origin
    // (merge policy: --delete-branch). A slot that once built any wi/ item
    // keeps a stale refs/remotes/origin/wi/* forever, and every later
    // force-with-lease push from that slot dies with "stale info" (observed
    // 2026-07-19: duncan wi/812dbdffc9df + three same-tick failures). Pruning
    // makes the lease reflect what origin actually has. Best-effort: if the
    // fetch fails the push may still succeed against an accurate ref.
    let _ = run_git(
        repo_path,
        ["fetch", "--prune", "origin"],
        Duration::from_secs(120),
    );
    // Some slot clones were provisioned single-branch (fetch refspec covering
    // only main — observed on the ring nodes 2026-07-19), so the plain fetch
    // above never learns the task branch's remote state and the lease check
    // below fails with "stale info" forever. Fetch the task branch explicitly
    // (tolerating absence: a first push has nothing to fetch).
    let _ = run_git(
        repo_path,
        [
            "fetch",
            "origin",
            &format!("+refs/heads/{task_branch}:refs/remotes/origin/{task_branch}"),
        ],
        Duration::from_secs(60),
    );
    // --force-with-lease: the harness OWNS wi/* branches. When a prior attempt
    // pushed and then died (daemon restart, timeout), the retry rebuilds fresh
    // history from origin/<base> and a plain push is rejected non-fast-forward
    // — the retry then fails despite correct code (observed twice 2026-07-17).
    // with-lease (not plain --force) still refuses to clobber a push we haven't
    // fetched, so a genuinely concurrent build on the same id can't be lost.
    run_git(
        repo_path,
        [
            OsStr::new("push"),
            OsStr::new("-u"),
            OsStr::new("--force-with-lease"),
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
    let out = match run_command_timeout(cmd, Duration::from_secs(120)) {
        Ok(o) => o,
        Err(e) if e.to_string().contains("already exists") => {
            // A retry force-pushes onto the SAME wi/ branch, so a PR opened by
            // a prior attempt is still there and now carries the new commits —
            // `gh pr create` then exits 1 with "a pull request … already
            // exists". That IS the desired end state; failing the item here
            // sheds finished work (observed 2026-07-19: PRs #840/#846 died
            // this way). Adopt the existing PR instead.
            let mut view = Command::new("gh");
            view.current_dir(worktree_path).args([
                "pr",
                "view",
                &worktree.task_branch,
                "--json",
                "url",
                "--jq",
                ".url",
            ]);
            if let Some(token) = crate::fleet_info::fetch_secret("github_gh_token").await {
                view.env("GH_TOKEN", token);
            }
            let vout = run_command_timeout(view, Duration::from_secs(60))?;
            let url = String::from_utf8_lossy(&vout.stdout).trim().to_string();
            if url.is_empty() {
                bail!(
                    "PR for {} already exists but its URL could not be resolved",
                    worktree.task_branch
                );
            }
            info!(
                work_item_id = %item.work_item_id,
                pr = %url,
                "work_item_dispatch: PR already exists for branch — adopting it"
            );
            return Ok(url);
        }
        Err(e) => return Err(e),
    };
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

#[doc(hidden)]
pub fn run_git<I, S>(cwd: &Path, args: I, timeout: Duration) -> Result<Output>
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

/// Like [`run_command_timeout`] but returns the `Output` for ANY exit code —
/// it only errors on spawn failure or timeout. Used for the backend CLI, whose
/// non-zero exits must be INSPECTED by the caller, not collapsed into a generic
/// "command failed": in particular `ff cli --require-change` exits 3 on a no-op
/// (the change already exists), which `run_ff_dispatch` must treat as "already
/// done", NOT as a backend failure to retry/fail. (Bailing on exit 3 here made
/// that handler dead code — an already-built feature task requeued to death.)
fn run_command_capture(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let program = command_display(&cmd);
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .with_context(|| format!("collect output for {program}"));
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command timed out after {timeout:?}: {program}");
        }
        std::thread::sleep(Duration::from_millis(COMMAND_POLL_MS));
    }
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
            bail!("command timed out after {timeout:?}: {program}");
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
        let worktree_removed =
            remove_worktree(&repo, &tree).is_ok() && (tree == repo || !tree.exists());
        // Clone-direct rows: the "worktree" is the slot's long-lived clone —
        // reclaiming would delete its target/node_modules out from under the
        // next build. Only legacy detached worktree dirs are reclaimed.
        if tree != repo {
            reclaimed_bytes = reclaimed_bytes.saturating_add(reclaim_build_artifacts(&tree));
        }
        let branch_deleted = run_git(
            &repo,
            [
                OsStr::new("branch"),
                OsStr::new("-D"),
                OsStr::new(&wt.task_branch),
            ],
            Duration::from_secs(30),
        )
        .is_ok();
        let mut tx = pg.begin().await?;
        sqlx::query(
            "UPDATE work_item_worktrees SET status = 'cleaned', cleaned_at = NOW() \
              WHERE work_item_id = $1",
        )
        .bind(wt.work_item_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO work_item_provenance
                 (work_item_id, cleanup_complete, cleanup_at, cleanup_detail)
             VALUES ($1, $2, CASE WHEN $2 THEN NOW() END,
                     jsonb_build_object('branch_deleted', $3, 'worktree_removed', $4))
             ON CONFLICT (work_item_id) DO UPDATE SET
                 cleanup_complete = EXCLUDED.cleanup_complete,
                 cleanup_at = EXCLUDED.cleanup_at,
                 cleanup_detail = EXCLUDED.cleanup_detail,
                 updated_at = NOW()",
        )
        .bind(wt.work_item_id)
        .bind(branch_deleted && worktree_removed)
        .bind(branch_deleted)
        .bind(worktree_removed)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE work_item_leases SET lease_state = 'released', released_at = NOW(), \
                    release_reason = 'workspace cleaned after terminal signal' \
              WHERE work_item_id = $1 AND released_at IS NULL",
        )
        .bind(wt.work_item_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE sub_agents SET current_work_item_id = NULL, status = 'idle', \
                    started_at = NULL, last_heartbeat_at = NOW() \
              WHERE current_work_item_id = $1",
        )
        .bind(wt.work_item_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
        AssignedWorkItem, DISPATCH_HOUSE_RULES, DispatchOutcome, LANE15_480B_PERMITS, ReviewerStat,
        affected_crate_manifests, agent_output_tail, backend_failed_without_output,
        builder_excludes_480b, classify_dispatch_outcome, command_display,
        complexity_at_least_moderate, default_clone_path, dispatch_budget_for_host,
        dispatch_prompt, expand_home, is_build_timeout, mirror_repo_url, order_cloud_reviewers,
        parse_cli_tokens, primary_or_default_backend, quick_empty_success_is_provider_failure,
        repo_cache_path, repo_slug, retry_error_is_actionable, rewrite_github_host_alias,
        same_model_family, should_attempt_lane15, status_output_is_clean, task_failed_alert_text,
        task_prefers_cloud_lane, try_acquire_lane15_480b_permit, use_local_lane,
    };
    use std::path::PathBuf;
    use std::time::Duration;
    use uuid::Uuid;

    #[test]
    fn builder_excludes_480b_for_local_builds_only() {
        // Rule 1: never the model that built the change. A local build may have
        // escalated to the 480B inside the codegen cascade, so any local
        // builder keeps the 480B out of the reviewer pool.
        assert!(builder_excludes_480b("local"));
        assert!(builder_excludes_480b("local:qwen3-coder-480b"));
        assert!(builder_excludes_480b("qwen3-coder-480b"));
        // Cloud builds leave the 480B eligible.
        assert!(!builder_excludes_480b("codex"));
        assert!(!builder_excludes_480b("claude"));
        assert!(!builder_excludes_480b("kimi"));
    }

    #[test]
    fn mirror_repo_url_appends_owner_repo_to_prefix() {
        // https-style prefix (trailing slash) + https GitHub URL.
        assert_eq!(
            mirror_repo_url(
                "https://git-mirror.local/",
                "https://github.com/venkatyarl/forge-fleet.git"
            )
            .as_deref(),
            Some("https://git-mirror.local/venkatyarl/forge-fleet")
        );
        // scp-style prefix (trailing colon) + ssh host-alias GitHub URL — the
        // shape every fleet slot actually has as its origin.
        assert_eq!(
            mirror_repo_url(
                "git@git-mirror.local:",
                "git@github.com-venkat:venkatyarl/forge-fleet.git"
            )
            .as_deref(),
            Some("git@git-mirror.local:venkatyarl/forge-fleet")
        );
        // Unparseable GitHub URL → no mirror URL (caller falls back to direct).
        assert_eq!(
            mirror_repo_url("https://git-mirror.local/", "not a url"),
            None
        );
    }

    #[test]
    fn lane15_requires_failure_enabled_gate_and_closed_breaker() {
        assert!(should_attempt_lane15(true, false, true, false));
        assert!(!should_attempt_lane15(false, false, true, false));
        assert!(!should_attempt_lane15(true, false, false, false));
        assert!(!should_attempt_lane15(true, false, true, true));
    }

    #[test]
    fn lane15_also_triggers_on_moderate_or_complex_even_without_lane1_failure() {
        // A moderate/complex task that never touched Lane 1 (e.g. it already
        // routed straight to cloud via prefers_cloud_lane) still gets one
        // Lane-1.5 480B shot before the cloud backstop.
        assert!(should_attempt_lane15(false, true, true, false));
        // Disabled gate or open breaker still block it, same as the failure path.
        assert!(!should_attempt_lane15(false, true, false, false));
        assert!(!should_attempt_lane15(false, true, true, true));
    }

    #[test]
    fn complexity_at_least_moderate_covers_moderate_and_complex_only() {
        assert!(!complexity_at_least_moderate("mechanical"));
        assert!(!complexity_at_least_moderate(""));
        assert!(complexity_at_least_moderate("moderate"));
        assert!(complexity_at_least_moderate("complex"));
    }

    #[test]
    fn order_cloud_reviewers_excludes_builder_and_weights_by_latency() {
        let backends = vec![
            "claude".to_string(),
            "codex".to_string(),
            "kimi".to_string(),
        ];
        let stat = |reviewer: &str, reviews: i64, avg: f64| ReviewerStat {
            reviewer: reviewer.to_string(),
            reviews,
            avg_latency_secs: avg,
        };

        // Rule 1: the builder never reviews its own change.
        let order = order_cloud_reviewers("codex", &[], &backends);
        assert_eq!(order, vec!["claude".to_string(), "kimi".to_string()]);
        assert!(same_model_family("cloud:codex", "codex"));
        assert!(
            !order_cloud_reviewers("cloud:codex", &[], &backends).contains(&"codex".to_string())
        );
        // A local builder excludes no cloud reviewer.
        let order = order_cloud_reviewers("local", &[], &backends);
        assert_eq!(order.len(), 3);

        // No history → declaration order preserved (all scores 0).
        let order = order_cloud_reviewers("", &[], &backends);
        assert_eq!(
            order,
            vec![
                "claude".to_string(),
                "codex".to_string(),
                "kimi".to_string()
            ]
        );

        // Weighted round-robin: score = reviews × avg latency. kimi is fast
        // (60s) and has done 2 reviews (score 120); claude is slow (300s) with
        // 1 review (score 300); codex has no history (score 0 → first, so the
        // fleet gathers data on it).
        let stats = [stat("claude", 1, 300.0), stat("kimi", 2, 60.0)];
        let order = order_cloud_reviewers("", &stats, &backends);
        assert_eq!(
            order,
            vec![
                "codex".to_string(),
                "kimi".to_string(),
                "claude".to_string()
            ]
        );
    }

    #[test]
    fn agent_output_tail_keeps_the_end_and_is_char_safe() {
        use std::os::unix::process::ExitStatusExt;
        let mk = |out: &str, err: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: out.as_bytes().to_vec(),
            stderr: err.as_bytes().to_vec(),
        };
        // Short output is returned whole (labelled).
        let whole = agent_output_tail(&mk("hello", "world"), 1500);
        assert!(whole.contains("[stdout]") && whole.contains("hello"));
        assert!(whole.contains("[stderr]") && whole.contains("world"));
        // Long output keeps the TAIL (where the final state/error lives) and
        // marks truncation, never panicking on a multi-byte boundary. The tail
        // comes from stderr (the trailing stream in the combined layout).
        let big = "é".repeat(5000); // multi-byte; 5000 chars, 10000 bytes
        let tail = agent_output_tail(&mk("", &big), 100);
        assert!(tail.starts_with('…'));
        assert_eq!(tail.chars().count(), 101); // 100 + the ellipsis
        assert!(tail.chars().all(|c| c == '…' || c == 'é'));
    }

    #[test]
    fn porcelain_status_cleanliness_ignores_only_whitespace() {
        use std::os::unix::process::ExitStatusExt;
        let output = |stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };

        assert!(status_output_is_clean(&output("\n")));
        assert!(!status_output_is_clean(&output(" M generated.rs\n")));
    }

    #[test]
    fn house_rules_forbid_the_agent_from_committing_or_opening_a_pr() {
        // #69 root cause: the old rule "open ONE PR and STOP" made codex
        // self-COMMIT its edits, leaving a CLEAN worktree — so the dispatch's
        // commit_worktree_changes saw dirty=false and DISCARDED the work as a
        // no-op (confirmed live on beyonce: codex committed d28c671, tree clean).
        // The harness commits + opens the PR; the agent must leave edits
        // UNCOMMITTED. Guard against a reword that reintroduces the trigger.
        assert!(
            !DISPATCH_HOUSE_RULES.contains("open ONE PR"),
            "house rules must NOT instruct the agent to open a PR (causes self-commit → discarded no-op)"
        );
        assert!(
            DISPATCH_HOUSE_RULES.contains("do NOT") && DISPATCH_HOUSE_RULES.contains("git commit"),
            "house rules must explicitly forbid the agent from running git commit"
        );
        assert!(
            DISPATCH_HOUSE_RULES.contains("UNCOMMITTED"),
            "house rules must tell the agent to leave edits UNCOMMITTED for the harness"
        );
    }

    #[tokio::test]
    async fn lane15_permit_or_skip_degrades_when_saturated() {
        use std::time::Duration;
        use tokio::sync::Semaphore;
        // A 2-permit ring modelling the single 480B instance's `--parallel 2`.
        let sem = Semaphore::new(super::LANE15_480B_PERMITS);
        let short = Duration::from_millis(50);

        // Both permits are free → two builds may hold the ring at once.
        let p1 = super::acquire_within(&sem, short).await;
        let p2 = super::acquire_within(&sem, short).await;
        assert!(p1.is_some() && p2.is_some(), "first 2 builds get a permit");

        // Ring saturated → a third build gets None (skip straight to cloud), and
        // does NOT block indefinitely — the timed wait returns.
        assert!(
            super::acquire_within(&sem, short).await.is_none(),
            "a 3rd build must skip (None), never queue behind the slow ring"
        );

        // Releasing a permit frees the ring for the next build.
        drop(p1);
        assert!(
            super::acquire_within(&sem, short).await.is_some(),
            "a freed permit lets the next build onto the ring"
        );
    }

    #[test]
    fn lane1_timeout_is_strictly_below_the_lease_stale_window() {
        // The Lane-1 local codegen MUST self-abort + fail over to the cloud
        // backstop before the stale-lease reaper can kill the lease mid-flight
        // (else the local-codegen breaker never learns and the task thrashes —
        // the beyonce/DGX "3 stalled attempts" failure). Guards against a future
        // edit that bumps either constant past the other.
        let lane1 = super::LANE1_TIMEOUT_SECS;
        let stale = crate::work_item_scheduler::LEASE_STALE_SECS as u64;
        assert!(
            lane1 < stale,
            "LANE1_TIMEOUT_SECS ({lane1}) must be < LEASE_STALE_SECS ({stale})",
        );
    }

    #[test]
    fn use_local_lane_gives_mechanical_local_tries_then_cloud() {
        // Mechanical (prefers_cloud=false), breaker closed: local lane is heartbeat-safe now
        // (#62/#792 moved blocking work off the async runtime), so it stays local for
        // LOCAL_LANE_MAX_TRIES=3 attempts before escalating to cloud.
        assert!(
            use_local_lane(0, false, false),
            "first attempt tries cheap local"
        );
        assert!(use_local_lane(1, false, false), "2nd attempt still local");
        assert!(use_local_lane(2, false, false), "3rd attempt still local");
        assert!(
            !use_local_lane(3, false, false),
            "past LOCAL_LANE_MAX_TRIES → cloud"
        );
        // A complexity-routed (complex or multi-file-heavy) task never touches the local lane.
        assert!(!use_local_lane(0, false, true));
        // Open local-codegen breaker → skip local even on attempt 0.
        assert!(!use_local_lane(0, true, false));
    }

    #[tokio::test]
    async fn lane15_permit_acquires_when_capacity_available() {
        let sem = tokio::sync::Semaphore::new(LANE15_480B_PERMITS);
        let permit =
            try_acquire_lane15_480b_permit(&sem, std::time::Duration::from_millis(10)).await;

        assert!(permit.is_some(), "free 480B lane capacity should run");
        assert_eq!(sem.available_permits(), LANE15_480B_PERMITS - 1);
    }

    #[tokio::test]
    async fn lane15_permit_skips_instead_of_queueing_when_full() {
        let sem = tokio::sync::Semaphore::new(LANE15_480B_PERMITS);
        let _held = sem
            .acquire_many(LANE15_480B_PERMITS as u32)
            .await
            .expect("test semaphore should be open");

        let permit =
            try_acquire_lane15_480b_permit(&sem, std::time::Duration::from_millis(1)).await;

        assert!(permit.is_none(), "busy 480B lane should skip to cloud");
        assert_eq!(sem.available_permits(), 0);
    }

    #[test]
    fn default_clone_path_lives_inside_the_slot() {
        // Build-path option A: the clone goes INSIDE the assigned sub-agent slot,
        // named by the repo — never the old shared top-level project-repos/.
        let p = default_clone_path(3, "git@github.com:venkatyarl/forge-fleet.git");
        let s = p.to_string_lossy();
        assert!(
            s.ends_with(".forgefleet/sub-agents/sub-agent-3/forge-fleet"),
            "expected sub-agent-3/forge-fleet, got {s}"
        );
        assert!(
            !s.contains("project-repos"),
            "must not use the old shared path: {s}"
        );
        // Different slots → different clones (per-slot isolation).
        let p5 = default_clone_path(5, "git@github.com:venkatyarl/forge-fleet.git");
        assert_ne!(p, p5);
        assert!(p5.to_string_lossy().contains("sub-agent-5/forge-fleet"));
        // Negative slot is clamped to 0 (never sub-agent--1).
        let pneg = default_clone_path(-1, "https://x/y/repo.git");
        assert!(pneg.to_string_lossy().contains("sub-agent-0/repo"));
    }

    #[test]
    fn repo_slug_derives_from_url_tail() {
        assert_eq!(
            repo_slug("git@github.com:venkatyarl/forge-fleet.git"),
            "forge-fleet"
        );
        assert_eq!(
            repo_slug("https://github.com/venkatyarl/forge-fleet.git"),
            "forge-fleet"
        );
        assert_eq!(repo_slug("https://x/y/repo.git"), "repo");
        // Unsafe characters become dashes and edge dashes are trimmed.
        assert_eq!(repo_slug("https://x/y/foo--bar.git"), "foo--bar");
        assert_eq!(repo_slug("https://x/y/-weird-.git"), "weird");
    }

    #[test]
    fn repo_cache_path_is_shared_and_slug_keyed() {
        let cache = repo_cache_path("git@github.com:venkatyarl/forge-fleet.git");
        let s = cache.to_string_lossy();
        assert!(
            s.ends_with(".forgefleet/cache/repos/forge-fleet"),
            "expected shared cache path, got {s}"
        );
        assert!(
            !s.contains("sub-agent"),
            "artifact cache must not be per-slot: {s}"
        );

        // The cache key must match the per-slot clone's repo slug so a cache hit
        // copies into the right destination.
        let slot = default_clone_path(2, "git@github.com:venkatyarl/forge-fleet.git");
        assert_eq!(slot.file_name(), cache.file_name());
        assert_ne!(slot, cache);
    }

    #[test]
    fn rewrite_github_host_alias_only_touches_bare_github() {
        // Bare scp-style github URL → rewritten to the canonical alias so the
        // clone (and the persisted origin) authenticates fleet-wide.
        assert_eq!(
            rewrite_github_host_alias(
                "git@github.com:venkatyarl/forge-fleet.git",
                "github.com-venkat"
            ),
            "git@github.com-venkat:venkatyarl/forge-fleet.git"
        );
        // Already-aliased host is left alone (idempotent — no double rewrite).
        assert_eq!(
            rewrite_github_host_alias(
                "git@github.com-venkat:venkatyarl/forge-fleet.git",
                "github.com-venkat"
            ),
            "git@github.com-venkat:venkatyarl/forge-fleet.git"
        );
        // https and non-github remotes are untouched.
        assert_eq!(
            rewrite_github_host_alias(
                "https://github.com/venkatyarl/forge-fleet.git",
                "github.com-venkat"
            ),
            "https://github.com/venkatyarl/forge-fleet.git"
        );
        assert_eq!(
            rewrite_github_host_alias("git@gitlab.com:acme/x.git", "github.com-venkat"),
            "git@gitlab.com:acme/x.git"
        );
    }

    #[test]
    fn expand_home_expands_leading_tilde_slash() {
        // SAFETY: this test only reads HOME; it does not mutate process env.
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                expand_home("~/.ssh/id_venkat"),
                format!("{home}/.ssh/id_venkat")
            );
        }
        // Absolute + bare paths are returned unchanged.
        assert_eq!(expand_home("/abs/id_key"), "/abs/id_key");
        assert_eq!(expand_home("id_key"), "id_key");
    }

    #[test]
    fn complexity_routes_hard_tasks_straight_to_cloud() {
        // Mechanical single-file → cheap local lane.
        assert!(!task_prefers_cloud_lane("mechanical", 1));
        assert!(!task_prefers_cloud_lane("mechanical", 0));
        // Moderate tasks that are not multi-file-heavy try local first.
        assert!(!task_prefers_cloud_lane("moderate", 1));
        assert!(!task_prefers_cloud_lane("moderate", 3));
        // Complex tasks bypass the local lane regardless of file count.
        assert!(task_prefers_cloud_lane("complex", 0));
        // Multi-file-heavy tasks (above the threshold) go cloud even if mechanical.
        assert!(task_prefers_cloud_lane("mechanical", 4));
        // Unknown/empty complexity is treated as mechanical (safe default).
        assert!(!task_prefers_cloud_lane("", 1));
    }

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
    use super::run_command_capture;
    use anyhow::anyhow;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Output};

    #[test]
    fn run_command_capture_returns_output_for_nonzero_exit() {
        // The whole point of `capture` vs `timeout`: a `--require-change` no-op
        // exits 3, and the caller must SEE that exit code — not receive an Err.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf hi; exit 3");
        let out = run_command_capture(cmd, std::time::Duration::from_secs(10))
            .expect("non-zero exit must be Ok, not Err");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"hi");
    }

    #[test]
    fn run_command_capture_returns_output_for_zero_exit() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 0");
        let out = run_command_capture(cmd, std::time::Duration::from_secs(10)).expect("clean run");
        assert!(out.status.success());
    }

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
    fn quick_empty_success_is_a_provider_failure() {
        let empty = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b" \n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(quick_empty_success_is_provider_failure(
            &empty,
            Duration::from_secs(12)
        ));
        assert!(!quick_empty_success_is_provider_failure(
            &empty,
            Duration::from_secs(31)
        ));

        let nonempty = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"done".to_vec(),
            stderr: Vec::new(),
        };
        assert!(!quick_empty_success_is_provider_failure(
            &nonempty,
            Duration::from_secs(1)
        ));
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
    fn quick_clean_empty_stdout_is_backend_failure() {
        assert!(backend_failed_without_output(
            false,
            true,
            b" \n\t",
            Duration::from_secs(12),
            false,
        ));
        assert!(!backend_failed_without_output(
            false,
            true,
            b"completed",
            Duration::from_secs(12),
            false,
        ));
    }

    #[test]
    fn killed_timeout_without_diff_is_backend_failure() {
        assert!(backend_failed_without_output(
            true,
            false,
            b"",
            Duration::from_secs(60),
            false,
        ));
        assert!(!backend_failed_without_output(
            true,
            false,
            b"",
            Duration::from_secs(60),
            true,
        ));
    }

    #[test]
    fn detects_build_timeout_through_error_context() {
        let timeout = anyhow!("command timed out after 300s").context("run cargo build");
        assert!(is_build_timeout(&timeout));
        assert!(is_build_timeout(&anyhow!(
            "max-build-duration exceeded after 1200s; build cancelled"
        )));
        assert!(!is_build_timeout(&anyhow!("cargo build exited 1")));
    }

    #[test]
    fn build_timeout_precedes_max_lease_age_backstop() {
        assert!(
            super::MAX_BUILD_DURATION_SECS
                < crate::work_item_scheduler::MAX_LEASE_DURATION_SECS as u64
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
    fn terminal_failure_alert_leads_with_human_context_and_puts_ids_last() {
        let item = AssignedWorkItem {
            work_item_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            project_id: "forge-fleet".into(),
            title: "Make Telegram alerts readable".into(),
            description: None,
            base_branch: None,
            repo_id: None,
            repo_url: None,
            repo_path: PathBuf::new(),
            sub_agent_id: Uuid::nil(),
            computer_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            computer_name: "build-mac".into(),
            session_id: Some(Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()),
            slot: 2,
            kind: "code".into(),
            attempts: 3,
            last_error: None,
            complexity: "mechanical".into(),
            predicted_paths_count: 1,
            brain_node_ids: Vec::new(),
            touched_paths: Vec::new(),
            context: serde_json::json!({"trigger": {"signature": "abc"}}),
            pre_work: vec!["reproduce".into()],
            work: vec!["fix root cause".into()],
            post_work: vec!["verify signal cleared".into()],
        };

        let alert = task_failed_alert_text(&item, "branch: raw-name\nstderr: compile failed");
        assert!(alert.starts_with("🛑 Make Telegram alerts readable"));
        assert!(alert.contains("Session: sub-agent-2 on build-mac"));
        assert!(alert.find("IDs (diagnostic)").unwrap() < alert.find("11111111-").unwrap());
        assert!(
            alert.find("Last error (diagnostic)").unwrap()
                < alert.find("IDs (diagnostic)").unwrap()
        );

        let prompt = dispatch_prompt(&item);
        assert!(prompt.contains("PRE_WORK — complete before editing:\n- reproduce"));
        assert!(prompt.contains("WORK — execute in order:\n- fix root cause"));
        assert!(
            prompt.contains("POST_WORK — complete after implementation:\n- verify signal cleared")
        );
        assert!(prompt.contains("\"signature\":\"abc\""));
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
    fn forced_fallback_defaults_to_claude_when_unrouted() {
        assert_eq!(primary_or_default_backend(&[]), "claude");
    }

    #[tokio::test]
    async fn self_verify_rejects_whitespace_only_added_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        super::run_git(repo, ["init"], Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["config", "user.name", "Test"],
            Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["config", "user.email", "test@example.com"],
            Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        super::run_git(repo, ["add", "-A"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["commit", "-m", "base"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["branch", "-M", "main"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["checkout", "-b", "task"], Duration::from_secs(10)).unwrap();
        std::fs::write(repo.join("empty.txt"), " \n\t").unwrap();
        super::run_git(repo, ["add", "-A"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["commit", "-m", "change"], Duration::from_secs(10)).unwrap();

        let error = super::self_verify_worktree(repo, "main").await.unwrap_err();
        assert!(error.contains("empty.txt"));
    }

    #[test]
    fn affected_tests_select_unique_changed_crate_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        super::run_git(repo, ["init"], Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["config", "user.name", "Test"],
            Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["config", "user.email", "test@example.com"],
            Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), "base").unwrap();
        super::run_git(repo, ["add", "-A"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["commit", "-m", "base"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["branch", "-M", "main"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["checkout", "-b", "task"], Duration::from_secs(10)).unwrap();
        std::fs::create_dir_all(repo.join("crates/demo/src")).unwrap();
        std::fs::write(
            repo.join("crates/demo/Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(repo.join("crates/demo/src/lib.rs"), "pub fn demo() {}\n").unwrap();
        super::run_git(repo, ["add", "-A"], Duration::from_secs(10)).unwrap();
        super::run_git(repo, ["commit", "-m", "change"], Duration::from_secs(10)).unwrap();

        assert_eq!(
            affected_crate_manifests(repo, "main").unwrap(),
            vec![PathBuf::from("crates/demo/Cargo.toml")]
        );
    }

    #[test]
    fn squash_adopt_retitles_self_commits_to_single_task_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        super::run_git(repo, ["init"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["config", "user.name", "Test"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["config", "user.email", "test@example.com"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        super::run_git(repo, ["add", "-A"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["commit", "-m", "base commit"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        // Make `main` the explicit base branch.
        super::run_git(
            repo,
            ["checkout", "-B", "main"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        // Harness creates the task branch at base; agent does its work on a
        // self-made branch.
        super::run_git(
            repo,
            ["checkout", "-B", "task", "main"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["checkout", "-b", "agent", "task"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        super::run_git(repo, ["add", "-A"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["commit", "-m", "agent commit 1"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        super::run_git(repo, ["add", "-A"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["commit", "-m", "agent commit 2"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();

        // The harness sees a clean tree and the task branch itself has no commits.
        assert!(!super::worktree_has_diff(repo));
        assert!(!super::branch_has_commits(repo, "main", "task").unwrap());

        let head =
            super::squash_adopt_worktree_head_onto_branch(repo, "main", "task", "Fix the thing")
                .unwrap();
        assert!(!head.is_empty());

        // Task branch now has exactly one commit ahead of base, retitled with the
        // work item title.
        assert!(super::branch_has_commits(repo, "main", "task").unwrap());
        let count = super::run_git(
            repo,
            ["rev-list", "--count", "main..task"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
        let log = super::run_git(
            repo,
            ["log", "-1", "--pretty=%s", "task"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let msg = String::from_utf8_lossy(&log.stdout);
        assert!(
            msg.contains("Fix the thing"),
            "commit should be retitled: {msg}"
        );
        // Changes from both agent commits are preserved.
        assert!(repo.join("a.txt").exists());
        assert!(repo.join("b.txt").exists());
    }

    #[test]
    fn squash_adopt_includes_uncommitted_dirty_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        super::run_git(repo, ["init"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["config", "user.name", "Test"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["config", "user.email", "test@example.com"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        super::run_git(repo, ["add", "-A"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["commit", "-m", "base commit"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["checkout", "-B", "main"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["checkout", "-B", "task", "main"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        super::run_git(
            repo,
            ["checkout", "-b", "agent", "task"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        std::fs::write(repo.join("committed.txt"), "committed").unwrap();
        super::run_git(repo, ["add", "-A"], std::time::Duration::from_secs(10)).unwrap();
        super::run_git(
            repo,
            ["commit", "-m", "agent commit"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        // And a dirty change the agent forgot to commit.
        std::fs::write(repo.join("dirty.txt"), "dirty").unwrap();
        assert!(super::worktree_has_diff(repo));

        super::squash_adopt_worktree_head_onto_branch(repo, "main", "task", "Fix with dirty")
            .unwrap();

        let log = super::run_git(
            repo,
            ["log", "-1", "--pretty=%s", "task"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("Fix with dirty"));
        let count = super::run_git(
            repo,
            ["rev-list", "--count", "main..task"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
        assert!(repo.join("committed.txt").exists());
        assert!(repo.join("dirty.txt").exists());
        assert!(!super::worktree_has_diff(repo));
    }
}
