use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, serde::Deserialize)]
struct LeafTask {
    title: String,
    description: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    complexity: Option<String>,
    /// Checkable "definition of done" the builder targets + the self-verify gate
    /// checks the diff against (the Anthropic long-running-agent feature-list
    /// pattern). Empty when the planner omitted it (older model / degraded).
    #[serde(default)]
    acceptance_criteria: Vec<String>,
}

pub async fn handle_pm(cmd: crate::PmCommand, cwd: Option<PathBuf>) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::PmCommand::List {
            project,
            status,
            assignee,
        } => {
            let rows: Vec<(
                uuid::Uuid,
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            )> = sqlx::query_as(
                "SELECT wi.id, wi.project_id, wi.kind, wi.title, wi.status, wi.priority, \
                        wi.assigned_to, wi.created_at \
                 FROM work_items wi \
                 WHERE ($1::text IS NULL OR wi.project_id = $1) \
                   AND ($2::text IS NULL OR wi.status = $2) \
                   AND ($3::text IS NULL OR wi.assigned_to = $3) \
                 ORDER BY wi.created_at DESC \
                 LIMIT 200",
            )
            .bind(project.as_deref())
            .bind(status.as_deref())
            .bind(assignee.as_deref())
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("list work items: {e}"))?;

            if rows.is_empty() {
                println!("(no work items)");
                return Ok(());
            }

            println!(
                "{:<38} {:<14} {:<6} {:<10} {:<8} {:<14} TITLE",
                "ID", "PROJECT", "KIND", "STATUS", "PRIORITY", "ASSIGNEE"
            );
            for (id, pid, kind, title, st, prio, asgn, _created) in rows {
                let title_clip = if title.chars().count() > 60 {
                    format!("{}…", title.chars().take(59).collect::<String>())
                } else {
                    title
                };
                println!(
                    "{:<38} {:<14} {:<6} {:<10} {:<8} {:<14} {}",
                    id.to_string(),
                    pid,
                    kind,
                    st,
                    prio,
                    asgn.as_deref().unwrap_or("-"),
                    title_clip,
                );
            }
        }
        crate::PmCommand::Create {
            project,
            kind,
            title,
            description,
            priority,
            repo,
        } => {
            // Validate project exists first so we give a clear error instead of an FK violation.
            let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM projects WHERE id = $1")
                .bind(&project)
                .fetch_optional(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("query project: {e}"))?;
            if exists.is_none() {
                return Err(anyhow::anyhow!(
                    "unknown project '{project}' — run `ff project seed` or check `ff project list`"
                ));
            }

            // A schedulable work item must carry an authoritative repository
            // binding. `--repo` resolves within this project's registered repos;
            // without it, only a single-repo project is unambiguous. Never infer
            // a different project's repo merely because the command was run from
            // that checkout.
            let repo_context = crate::repo_context::resolve_required_project_repo(
                &pool,
                &project,
                cwd.clone(),
                repo.as_deref(),
            )
            .await?;
            let created_by = ff_agent::fleet_info::resolve_this_worker_name().await;
            let prio = priority.unwrap_or_else(|| "normal".to_string());
            let row: (uuid::Uuid,) = sqlx::query_as(
                "INSERT INTO work_items \
                    (project_id, kind, title, description, priority, created_by, \
                     repo_id, repo_url, repo_path) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 RETURNING id",
            )
            .bind(&project)
            .bind(&kind)
            .bind(&title)
            .bind(description.as_deref())
            .bind(&prio)
            .bind(&created_by)
            .bind(repo_context.repo_id)
            .bind(repo_context.repo_url.as_deref())
            .bind(
                repo_context
                    .repo_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
            )
            .fetch_one(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert work item: {e}"))?;

            println!("{GREEN}✓ Created work item{RESET}");
            println!("  id:       {}", row.0);
            println!("  project:  {project}");
            println!("  kind:     {kind}");
            println!("  title:    {title}");
            println!("  priority: {prio}");
            println!(
                "  repo_id:  {}",
                repo_context
                    .repo_id
                    .map(|id| id.to_string())
                    .as_deref()
                    .unwrap_or("-")
            );
            println!(
                "  repo_url: {}",
                repo_context.repo_url.as_deref().unwrap_or("-")
            );
            println!("  created_by: {created_by}");
        }
        crate::PmCommand::Close {
            id,
            evidence,
            commit,
            verified,
            deploy_verified,
        } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid work item id '{id}': {e}"))?;
            let actor = ff_agent::fleet_info::resolve_this_worker_name().await;
            let result = close_work_item(
                &pool,
                uid,
                &evidence,
                commit.as_deref(),
                verified,
                deploy_verified,
                &actor,
            )
            .await?;
            println!("{GREEN}✓ work item {id} closed with evidence{RESET}");
            println!("  status: {}", result.status);
            println!("  actor:  {actor}");
        }
        crate::PmCommand::Decompose {
            goal,
            project,
            llm,
            repo,
            ready,
            max,
        } => {
            handle_pm_decompose(&pool, goal, project, llm, repo, cwd, ready, max).await?;
        }
        crate::PmCommand::Ready { id, on } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid work item id '{id}': {e}"))?;
            let row: Option<(String,)> = sqlx::query_as(
                "UPDATE work_items \
                    SET status = 'ready', \
                        assigned_computer = COALESCE($2, assigned_computer) \
                  WHERE id = $1 \
                 RETURNING kind",
            )
            .bind(uid)
            .bind(on.as_deref())
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("flag ready: {e}"))?;
            match row {
                None => return Err(anyhow::anyhow!("no work item with id {id}")),
                Some((kind,)) => {
                    println!("{GREEN}✓ work item {id} flagged ready{RESET}");
                    if let Some(host) = &on {
                        println!("  pinned to: {host}");
                    }
                    if kind == "task" {
                        println!(
                            "  the Pillar 4 scheduler (10s tick) will assign it to a free slot."
                        );
                    } else {
                        println!(
                            "  note: kind='{kind}' — only kind='task' (leaf) items are scheduled; \
                             decompose into tasks first."
                        );
                    }
                }
            }
        }
        crate::PmCommand::Retry { id, on } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid work item id '{id}': {e}"))?;
            // Gate on terminal-but-retryable states so a LIVE item's attempts
            // are never reset mid-flight (that would defeat the failure ceiling).
            let row: Option<(String,)> = sqlx::query_as(
                "UPDATE work_items \
                    SET status = 'ready', \
                        attempts = 0, \
                        last_error = NULL, \
                        assigned_computer = COALESCE($2, assigned_computer) \
                  WHERE id = $1 \
                    AND status IN ('failed', 'cancelled', 'blocked') \
                    AND NOT EXISTS ( \
                        SELECT 1 FROM work_item_leases l \
                         WHERE l.work_item_id = work_items.id \
                           AND l.released_at IS NULL \
                    ) \
                    AND NOT EXISTS ( \
                        SELECT 1 FROM sub_agents sa \
                         WHERE sa.current_work_item_id = work_items.id \
                    ) \
                 RETURNING kind",
            )
            .bind(uid)
            .bind(on.as_deref())
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("retry: {e}"))?;
            match row {
                None => {
                    let state: Option<(String, bool, bool)> = sqlx::query_as(
                        "SELECT w.status, \
                                EXISTS(SELECT 1 FROM work_item_leases l \
                                        WHERE l.work_item_id = w.id AND l.released_at IS NULL), \
                                EXISTS(SELECT 1 FROM sub_agents sa \
                                        WHERE sa.current_work_item_id = w.id) \
                           FROM work_items w WHERE w.id = $1",
                    )
                    .bind(uid)
                    .fetch_optional(&pool)
                    .await?;
                    if let Some((status, live_lease, owned_slot)) = state
                        && (live_lease || owned_slot)
                    {
                        return Err(anyhow::anyhow!(
                            "work item {id} is '{status}' but its prior lease/slot is still owned; \
                             retry is fenced until the cancelling worker reaps its backend and releases ownership"
                        ));
                    }
                    return Err(anyhow::anyhow!(
                        "no retryable work item with id {id} — it must be failed/cancelled/blocked \
                         (use `ff pm ready` for a fresh/idea item; a live item can't be reset)"
                    ));
                }
                Some((kind,)) => {
                    println!(
                        "{GREEN}✓ work item {id} reset (attempts=0, error cleared) and flagged ready for retry{RESET}"
                    );
                    if let Some(host) = &on {
                        println!("  pinned to: {host}");
                    }
                    if kind != "task" {
                        println!(
                            "  note: kind='{kind}' — only kind='task' (leaf) items are scheduled."
                        );
                    }
                }
            }
        }
        crate::PmCommand::BindRepo { id, repo } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid work item id '{id}': {e}"))?;
            let actor = ff_agent::fleet_info::resolve_this_worker_name().await;
            let result = bind_work_item_repo(&pool, uid, &repo, &actor).await?;
            if result.already_bound {
                println!(
                    "{YELLOW}work item {id} is already bound to {} ({}){RESET}",
                    result.repo_name, result.repo_id
                );
            } else {
                println!(
                    "{GREEN}✓ bound work item {id} to {} ({}){RESET}",
                    result.repo_name, result.repo_id
                );
                println!("  repo_url: {}", result.repo_url);
                println!("  status:   {} (unchanged)", result.status);
            }
        }
        crate::PmCommand::Board { limit } => {
            // Fleet-wide rollup first — the at-a-glance state of distributed
            // concurrent dev (Pillar 4) across all computers, before the detail.
            print_board_summary(&pool).await?;
            // The autonomous build pipeline at a glance: work_items joined with
            // their live lease (host), worktree status, and merge-queue/PR state.
            // HOST resolves to the LIVE lease's computer (who's actually building
            // right now) when there's an un-released, un-expired lease, falling
            // back to the planned w.assigned_computer otherwise. live_host being
            // non-NULL is how the print loop flags an actively-building row.
            let rows: Vec<(
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                bool,
            )> = sqlx::query_as(
                "SELECT w.kind, w.title, w.status, w.assigned_computer, lc.name AS live_host, \
                        wt.status AS worktree, mq.status AS merge_q, w.pr_url, w.repo_url, w.repo_path, \
                        EXISTS (SELECT 1 FROM work_items c WHERE c.parent_id = w.id) AS is_parent \
                   FROM work_items w \
                   LEFT JOIN work_item_worktrees wt \
                          ON wt.work_item_id = w.id AND wt.status <> 'cleaned' \
                   LEFT JOIN work_item_merge_queue mq ON mq.work_item_id = w.id \
                   LEFT JOIN work_item_leases l \
                          ON l.work_item_id = w.id \
                         AND l.released_at IS NULL AND l.lease_expires_at > NOW() \
                   LEFT JOIN computers lc ON lc.id = l.computer_id \
                  WHERE w.status NOT IN ('idea', 'cancelled') OR w.pr_url IS NOT NULL \
                  ORDER BY w.created_at DESC \
                  LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query board: {e}"))?;

            if rows.is_empty() {
                println!("(no active work items — flag one with `ff pm ready <id>`)");
                return Ok(());
            }
            println!(
                "{CYAN}{:<8} {:<34} {:<11} {:<8} {:<11} {:<18} PR{RESET}",
                "KIND", "TITLE", "STATUS", "HOST", "MERGE-Q", "REPO"
            );
            for (
                kind,
                title,
                status,
                assigned_host,
                live_host,
                _worktree,
                merge_q,
                pr,
                repo_url,
                repo_path,
                is_parent,
            ) in rows
            {
                let t: String = title.chars().take(33).collect();
                let pr_short = pr
                    .as_deref()
                    .and_then(|u| u.rsplit('/').next())
                    .map(|n| format!("#{n}"))
                    .unwrap_or_default();
                // Prefer the live lease's computer (●  = building right now); else
                // the planned assignment.
                let host = match live_host {
                    Some(h) => format!("{h}\u{25cf}"),
                    None => assigned_host.unwrap_or_default(),
                };
                let repo_hint = repo_path
                    .as_deref()
                    .or(repo_url.as_deref())
                    .and_then(|s| s.rsplit('/').next())
                    .unwrap_or("");
                // Parent work_items are not schedulable leaves; mark them so the
                // board doesn't look like starved ready work.
                let kind_label = if is_parent { format!("{kind}*") } else { kind };
                println!(
                    "{:<8} {:<34} {:<11} {:<8} {:<11} {:<18} {}",
                    kind_label,
                    t,
                    status,
                    host,
                    merge_q.unwrap_or_default(),
                    repo_hint,
                    pr_short
                );
            }
        }
        crate::PmCommand::Doctor => {
            print_pm_doctor(&pool).await?;
        }
        crate::PmCommand::Stats { json } => {
            let stats = collect_pm_stats(&pool).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                print!("{}", render_pm_stats(&stats));
            }
        }
        crate::PmCommand::Summary(args) => {
            let stats = collect_pm_stats(&pool).await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                print!("{}", render_pm_stats(&stats));
            }
        }
        crate::PmCommand::Velocity => {
            print!("{}", ff_agent::pm_velocity::velocity_digest(&pool).await?);
        }
        crate::PmCommand::Purge {
            kind,
            status,
            project,
            older_than,
            yes,
            json,
        } => {
            handle_pm_purge(
                &pool,
                kind.as_deref(),
                status.as_deref(),
                project.as_deref(),
                older_than.as_deref(),
                yes,
                json,
            )
            .await?;
        }
        crate::PmCommand::Cancel { id } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid work item id '{id}': {e}"))?;
            let mut tx = pool.begin().await?;
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM work_items WHERE id = $1 FOR UPDATE")
                    .bind(uid)
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some(status) = status else {
                tx.rollback().await?;
                return Err(anyhow::anyhow!("no work item with id {id}"));
            };
            if matches!(status.as_str(), "done" | "merged") {
                tx.rollback().await?;
                return Err(anyhow::anyhow!(
                    "work item {id} is already terminal '{status}' and cannot be cancelled"
                ));
            }
            let backend_may_be_running = status == "building";
            sqlx::query(
                "UPDATE work_items SET status = 'cancelled' \
                  WHERE id = $1 AND status NOT IN ('done', 'merged', 'cancelled')",
            )
            .bind(uid)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("cancel work item: {e}"))?;

            // A claimed/ready/review item cannot own a running backend. Release
            // those leases atomically here. A BUILDING item is different: status
            // is only the cancellation intent signal, and the spawning worker
            // retains the slot until it TERM/KILLs and reaps its exact process.
            if !backend_may_be_running {
                sqlx::query(
                    "WITH released AS (
                         UPDATE work_item_leases
                            SET released_at = NOW(),
                                lease_state = 'released',
                                release_reason = 'cancelled before backend ownership'
                          WHERE work_item_id = $1
                            AND released_at IS NULL
                      RETURNING sub_agent_id, computer_id, work_item_id
                     )
                     UPDATE sub_agents sa
                        SET current_work_item_id = NULL,
                            status = 'idle',
                            started_at = NULL,
                            last_heartbeat_at = NOW()
                       FROM released r
                      WHERE sa.id = r.sub_agent_id
                        AND sa.computer_id = r.computer_id
                        AND sa.current_work_item_id = r.work_item_id",
                )
                .bind(uid)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("release non-building cancellation: {e}"))?;
            }
            // Drop it out of the merge queue so the drain stops considering it
            // (the per-host worktree reaper cleans the on-disk worktree).
            let _ = sqlx::query(
                "UPDATE work_item_merge_queue \
                    SET status = 'failed', failed_at = NOW(), failure_reason = 'work item cancelled' \
                  WHERE work_item_id = $1 AND status NOT IN ('merged')",
            )
            .bind(uid)
            .execute(&mut *tx)
            .await;
            tx.commit().await?;

            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            let released = loop {
                let owned: bool = sqlx::query_scalar(
                    "SELECT EXISTS(
                        SELECT 1 FROM work_item_leases
                         WHERE work_item_id = $1 AND released_at IS NULL
                        UNION ALL
                        SELECT 1 FROM sub_agents
                         WHERE current_work_item_id = $1
                    )",
                )
                .bind(uid)
                .fetch_one(&pool)
                .await?;
                if !owned {
                    break true;
                }
                if tokio::time::Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            };
            if released && backend_may_be_running {
                println!(
                    "{GREEN}✓ work item {id} cancelled{RESET} (backend reaped, lease released, slot freed)"
                );
            } else if released {
                println!(
                    "{GREEN}✓ work item {id} cancelled{RESET} (no running backend; lease released, slot freed)"
                );
            } else {
                println!(
                    "{YELLOW}⚠ work item {id} cancellation accepted{RESET}; exact worker termination/reap is still pending"
                );
            }
        }
        crate::PmCommand::Show { id } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid UUID '{id}': {e}"))?;
            let row: Option<(
                uuid::Uuid,
                String,
                String,
                String,
                Option<String>,
                String,
                String,
                Option<String>,
                Option<String>,
                String,
                chrono::DateTime<chrono::Utc>,
                Option<uuid::Uuid>,
                Option<String>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT id, project_id, kind, title, description, status, priority, \
                        assigned_to, assigned_computer, created_by, created_at, repo_id, repo_url, repo_path \
                 FROM work_items WHERE id = $1",
            )
            .bind(uid)
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query work item: {e}"))?;

            let Some((
                id,
                pid,
                kind,
                title,
                desc,
                status,
                prio,
                asgn,
                computer,
                created_by,
                created_at,
                repo_id,
                repo_url,
                repo_path,
            )) = row
            else {
                return Err(anyhow::anyhow!("work item {uid} not found"));
            };

            println!("{CYAN}Work item{RESET} {id}");
            println!("  project:      {pid}");
            println!("  kind:         {kind}");
            println!("  title:        {title}");
            if let Some(d) = desc.as_deref() {
                println!("  description:  {d}");
            }
            println!("  status:       {status}");
            println!("  priority:     {prio}");
            println!("  assigned_to:  {}", asgn.as_deref().unwrap_or("-"));
            println!("  computer:     {}", computer.as_deref().unwrap_or("-"));
            println!(
                "  repo_id:      {}",
                repo_id.map(|id| id.to_string()).as_deref().unwrap_or("-")
            );
            println!("  repo_url:     {}", repo_url.as_deref().unwrap_or("-"));
            println!("  repo_path:    {}", repo_path.as_deref().unwrap_or("-"));
            println!("  created_by:   {created_by}");
            println!(
                "  created_at:   {}",
                created_at.format("%Y-%m-%d %H:%M UTC")
            );
            let provenance: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT to_jsonb(p) FROM work_item_provenance p WHERE work_item_id = $1",
            )
            .bind(uid)
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query work-item provenance: {e}"))?;
            if let Some(provenance) = provenance {
                println!("  provenance:   {}", serde_json::to_string(&provenance)?);
            }

            let outputs: Vec<(
                uuid::Uuid,
                String,
                Option<String>,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            )> = sqlx::query_as(
                "SELECT id, kind, title, file_path, produced_at \
                     FROM work_outputs WHERE work_item_id = $1 \
                     ORDER BY produced_at DESC",
            )
            .bind(uid)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query work outputs: {e}"))?;

            if !outputs.is_empty() {
                println!();
                println!("{CYAN}Outputs ({}){RESET}", outputs.len());
                for (oid, okind, otitle, opath, oat) in outputs {
                    println!(
                        "  {} [{okind}] {} {} — {}",
                        oid,
                        otitle.as_deref().unwrap_or("-"),
                        opath.as_deref().unwrap_or("-"),
                        oat.format("%Y-%m-%d %H:%M UTC"),
                    );
                }
            }
        }
        crate::PmCommand::ImportClaudeTasks {
            session,
            project,
            dry_run,
        } => {
            handle_pm_import_claude_tasks(&pool, session, &project, dry_run).await?;
        }
        crate::PmCommand::Integrate { branches, base } => {
            handle_pm_integrate(cwd, branches, base).await?;
        }
    }
    Ok(())
}

const REPOSITORY_RESOLUTION_FAILED_CLOSED_PREFIX: &str = "repository resolution failed closed: ";

#[derive(Debug, PartialEq, Eq)]
struct BindRepoResult {
    repo_id: uuid::Uuid,
    repo_name: String,
    repo_url: String,
    status: String,
    already_bound: bool,
}

fn is_repository_resolution_failed_closed(error: &str) -> bool {
    error
        .strip_prefix(REPOSITORY_RESOLUTION_FAILED_CLOSED_PREFIX)
        .is_some_and(|detail| !detail.trim().is_empty())
}

async fn bind_work_item_repo(
    pool: &sqlx::PgPool,
    id: uuid::Uuid,
    selector: &str,
    actor: &str,
) -> Result<BindRepoResult> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("repository selector must not be empty");
    }

    let mut tx = pool.begin().await?;
    let item: Option<(
        String,
        String,
        Option<String>,
        Option<uuid::Uuid>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT project_id, status, last_error, repo_id, repo_url \
           FROM work_items \
          WHERE id = $1 \
          FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((project_id, status, last_error, existing_repo_id, existing_repo_url)) = item else {
        anyhow::bail!("no work item with id {id}");
    };

    // UUID lookup is deliberately project-scoped, so a valid UUID owned by a
    // different project is indistinguishable from an unknown selector.
    // A syntactically valid UUID is always an ID selector, never also a name
    // selector. This avoids a contrived UUID-shaped name making a valid ID
    // ambiguous while preserving exact-name ambiguity detection.
    let repos: Vec<(uuid::Uuid, String, String)> =
        if let Ok(selector_id) = uuid::Uuid::parse_str(selector) {
            sqlx::query_as(
                "SELECT id, COALESCE(name, id::text), github_url \
                   FROM project_repos \
                  WHERE project_id = $1 AND id = $2",
            )
            .bind(&project_id)
            .bind(selector_id)
            .fetch_all(&mut *tx)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, COALESCE(name, id::text), github_url \
                   FROM project_repos \
                  WHERE project_id = $1 AND name = $2 \
                  ORDER BY id",
            )
            .bind(&project_id)
            .bind(selector)
            .fetch_all(&mut *tx)
            .await?
        };
    let (repo_id, repo_name, repo_url) = match repos.as_slice() {
        [only] => only.clone(),
        [] => anyhow::bail!(
            "unknown repository '{selector}' in project '{project_id}' \
             (expected a project_repos UUID or exact name)"
        ),
        many => anyhow::bail!(
            "repository name '{selector}' is ambiguous in project '{project_id}' \
             ({} matches); use the project_repos UUID",
            many.len()
        ),
    };
    if repo_url.trim().is_empty() {
        anyhow::bail!("repository {repo_id} in project '{project_id}' has a blank github_url");
    }

    if let Some(bound_id) = existing_repo_id {
        if bound_id != repo_id {
            anyhow::bail!(
                "work item {id} is already bound to different repository {bound_id}; refusing overwrite"
            );
        }
        if existing_repo_url.as_deref() != Some(repo_url.as_str()) {
            anyhow::bail!(
                "work item {id} has repository {repo_id} but a non-canonical repo_url; \
                 refusing to mutate an existing binding"
            );
        }
        tx.rollback().await?;
        return Ok(BindRepoResult {
            repo_id,
            repo_name,
            repo_url,
            status,
            already_bound: true,
        });
    }
    if existing_repo_url
        .as_deref()
        .is_some_and(|url| !url.is_empty())
    {
        anyhow::bail!(
            "work item {id} already has repo_url '{}' without repo_id; refusing overwrite",
            existing_repo_url.as_deref().unwrap_or_default()
        );
    }

    let repairable_error = last_error
        .as_deref()
        .is_some_and(is_repository_resolution_failed_closed);
    if status != "ready" {
        anyhow::bail!("work item {id} is '{status}'; bind-repo only permits unbound ready items");
    }

    let has_live_lease: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM work_item_leases \
                        WHERE work_item_id = $1 AND released_at IS NULL)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if has_live_lease {
        anyhow::bail!(
            "work item {id} has an unreleased lease (including an expired lease pending reaping)"
        );
    }
    let has_slot: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sub_agents WHERE current_work_item_id = $1)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if has_slot {
        anyhow::bail!("work item {id} is still assigned to a live sub-agent slot");
    }

    let clear_repair_error = repairable_error;
    let updated = sqlx::query(
        "UPDATE work_items \
            SET repo_id = $2, repo_url = $3, \
                last_error = CASE WHEN $4 THEN NULL ELSE last_error END \
          WHERE id = $1 AND repo_id IS NULL \
            AND (repo_url IS NULL OR repo_url = '') \
            AND status = $5",
    )
    .bind(id)
    .bind(repo_id)
    .bind(&repo_url)
    .bind(clear_repair_error)
    .bind(&status)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!("work item {id} changed concurrently; repository binding rolled back");
    }

    sqlx::query(
        "INSERT INTO work_item_events \
            (work_item_id, from_status, to_status, computer, attempt, detail) \
         SELECT id, status, 'repo_bound', $2, attempts, \
                jsonb_build_object('action', 'repo_bound', 'repo_id', $3::text, \
                                   'repo_url', $4::text, 'repo_name', $5::text) \
           FROM work_items WHERE id = $1",
    )
    .bind(id)
    .bind(actor)
    .bind(repo_id)
    .bind(&repo_url)
    .bind(&repo_name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(BindRepoResult {
        repo_id,
        repo_name,
        repo_url,
        status,
        already_bound: false,
    })
}

#[derive(Debug)]
struct CloseResult {
    status: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HumanCloseEvidence {
    at: chrono::DateTime<chrono::Utc>,
    actor: String,
    evidence: String,
}

fn validate_close_request<'a>(
    evidence: &'a str,
    commit: Option<&'a str>,
    verified: bool,
    deploy_verified: bool,
) -> Result<(&'a str, Option<&'a str>)> {
    let evidence = evidence.trim();
    if evidence.is_empty() {
        anyhow::bail!("--evidence must not be blank");
    }
    if deploy_verified && !verified {
        anyhow::bail!("--deploy-verified requires --verified");
    }
    let commit = commit.map(str::trim);
    if commit == Some("") {
        anyhow::bail!("--commit must not be blank");
    }
    Ok((evidence, commit))
}

fn append_human_close_evidence(
    acceptance_check: Option<&str>,
    last_error: Option<&str>,
    actor: &str,
    evidence: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(String, bool)> {
    const PREFIX: &str = "human-close: ";
    let mut out = acceptance_check.unwrap_or_default().trim_end().to_string();
    if let Some(last_error) = last_error.filter(|value| !value.trim().is_empty()) {
        let preserved = format!("prior-last-error: {}", last_error.trim());
        if !out.lines().any(|line| line == preserved) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&preserved);
        }
    }

    let duplicate = out
        .lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .any(|json| {
            serde_json::from_str::<HumanCloseEvidence>(json)
                .map(|old| old.evidence == evidence)
                .unwrap_or(false)
        });
    if duplicate {
        return Ok((out, false));
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(PREFIX);
    out.push_str(&serde_json::to_string(&HumanCloseEvidence {
        at,
        actor: actor.to_string(),
        evidence: evidence.to_string(),
    })?);
    Ok((out, true))
}

async fn close_work_item(
    pool: &sqlx::PgPool,
    id: uuid::Uuid,
    evidence: &str,
    commit: Option<&str>,
    verified: bool,
    deploy_verified: bool,
    actor: &str,
) -> Result<CloseResult> {
    let (evidence, commit) = validate_close_request(evidence, commit, verified, deploy_verified)?;
    let mut tx = pool.begin().await?;
    let row: Option<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i16,
        i32,
    )> = sqlx::query_as(
        "SELECT status, acceptance_check, last_error, fix_commit, verified, \
                COALESCE(deploy_verified, 0) \
           FROM work_items WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("lock work item: {error}"))?;
    let Some((
        source_status,
        acceptance_check,
        last_error,
        existing_commit,
        existing_verified,
        existing_deploy_verified,
    )) = row
    else {
        anyhow::bail!("no work item with id {id}");
    };

    match source_status.as_str() {
        "building" | "claimed" | "in_review" | "decomposing" => {
            anyhow::bail!("work item {id} is active ({source_status}) and cannot be closed");
        }
        "idea" | "backlog" | "ready" | "blocked" | "failed" | "cancelled" | "decomposed"
        | "in_progress" | "done" | "merged" => {}
        other => anyhow::bail!("work item {id} has unsupported close source status '{other}'"),
    }
    // Treat every unreleased lease as active for closure purposes, even after
    // its timestamp expires. Otherwise a concurrent heartbeat could revive an
    // expired row after this check and leave a terminal item with a live slot.
    // The lease reaper must release stale rows before evidence closure.
    let unreleased_lease: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM work_item_leases \
          WHERE work_item_id = $1 AND released_at IS NULL)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("check active lease: {error}"))?;
    if unreleased_lease {
        anyhow::bail!("work item {id} has an unreleased lease and cannot be closed");
    }
    if let (Some(existing), Some(requested)) = (existing_commit.as_deref(), commit)
        && !existing.trim().is_empty()
        && existing.trim() != requested
    {
        anyhow::bail!(
            "work item {id} already records fix_commit '{existing}', conflicting with '{requested}'"
        );
    }

    let now = chrono::Utc::now();
    let prior_acceptance = acceptance_check.as_deref().unwrap_or_default().trim_end();
    let (acceptance_check, _) = append_human_close_evidence(
        acceptance_check.as_deref(),
        last_error.as_deref(),
        actor,
        evidence,
        now,
    )?;
    let acceptance_changed = acceptance_check != prior_acceptance;
    let target_status = if matches!(source_status.as_str(), "done" | "merged") {
        source_status.as_str()
    } else {
        "done"
    };
    let already_idempotent = !acceptance_changed
        && target_status == source_status
        && commit.map_or(true, |requested| {
            existing_commit
                .as_deref()
                .is_some_and(|existing| existing.trim() == requested)
        })
        && last_error
            .as_deref()
            .map_or(true, |error| error.trim().is_empty())
        && (!verified || existing_verified != 0)
        && (!deploy_verified || existing_deploy_verified != 0);
    if already_idempotent {
        tx.rollback().await?;
        return Ok(CloseResult {
            status: source_status,
        });
    }

    let updated: Option<(String,)> = sqlx::query_as(
        "UPDATE work_items \
            SET status = $3, acceptance_check = $4, last_error = NULL, \
                completed_at = COALESCE(completed_at, $5), \
                fix_commit = COALESCE(NULLIF(fix_commit, ''), $6), \
                verified = CASE WHEN $7 THEN 1 ELSE verified END, \
                verified_at = CASE WHEN $7 THEN COALESCE(verified_at, $5) ELSE verified_at END, \
                deploy_verified = CASE WHEN $8 THEN 1 ELSE deploy_verified END \
          WHERE id = $1 AND status = $2 \
        RETURNING status",
    )
    .bind(id)
    .bind(&source_status)
    .bind(target_status)
    .bind(&acceptance_check)
    .bind(now)
    .bind(commit)
    .bind(verified)
    .bind(deploy_verified)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("close work item: {error}"))?;
    let Some((status,)) = updated else {
        anyhow::bail!("work item {id} changed status concurrently; closure rolled back");
    };
    sqlx::query(
        "INSERT INTO work_item_events \
            (work_item_id, from_status, to_status, computer, detail) \
         VALUES ($1, $2, $3, $4, jsonb_build_object( \
            'action', 'human_close', 'evidence', $5::text, 'commit', $6::text, \
            'verified', $7::boolean, 'deploy_verified', $8::boolean))",
    )
    .bind(id)
    .bind(&source_status)
    .bind(&status)
    .bind(actor)
    .bind(evidence)
    .bind(commit)
    .bind(verified)
    .bind(deploy_verified)
    .execute(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("audit work item close: {error}"))?;
    tx.commit().await?;
    Ok(CloseResult { status })
}

/// `ff pm integrate` — build an integration branch by merging a cluster of
/// fleet PR branches onto a base, in a throwaway git worktree so the operator's
/// working tree is never disturbed. Reports which children merge clean vs
/// conflict; leaves the integration branch in the worktree only when clean so
/// the operator can `git -C <wt> push` it.
async fn handle_pm_integrate(
    cwd: Option<PathBuf>,
    branches: Vec<String>,
    base: String,
) -> Result<()> {
    use ff_agent::pr_integration::IntegrationPlan;
    use ff_agent::pr_integration_branch::{ChildOutcome, build_integration_branch};

    if branches.is_empty() {
        anyhow::bail!("pass --branches <a,b,c> (comma-separated fleet PR branches)");
    }

    // Locate the repo root so `git worktree add` runs against the right repo.
    let start = cwd.unwrap_or(std::env::current_dir()?);
    let root = git_repo_root(&start)?;

    // Throwaway worktree. Detach so it doesn't need a fresh branch, and use a
    // unique path so concurrent runs don't collide.
    let wt = root.join(format!(
        ".ff-integrate-{}",
        std::process::id() // pid — unique per invocation, no clock needed
    ));
    let wt_str = wt.to_string_lossy().to_string();
    run_git(&root, &["worktree", "add", "--detach", "--quiet", &wt_str])
        .map_err(|e| anyhow::anyhow!("create integration worktree: {e}"))?;

    let plan = IntegrationPlan {
        child_branches: branches.clone(),
        pr_numbers: vec![],
        target_branch: base.clone(),
    };

    let outcome = build_integration_branch(&wt, &plan).await;

    // Always report, then clean up the worktree unless the run was clean (a
    // clean integration branch is worth keeping so the operator can push it).
    match &outcome {
        Ok(o) => {
            println!(
                "{CYAN}Integration: {} ← {}{RESET}",
                o.integration_branch, base
            );
            for (br, res) in &o.results {
                let (mark, detail) = match res {
                    ChildOutcome::Merged => {
                        (format!("{GREEN}✓{RESET}"), "merged clean".to_string())
                    }
                    ChildOutcome::Conflicted(files) => (
                        format!("{RED}✗{RESET}"),
                        format!("CONFLICT: {}", files.join(", ")),
                    ),
                    ChildOutcome::Missing => {
                        (format!("{YELLOW}?{RESET}"), "branch not found".to_string())
                    }
                };
                println!("  {mark} {br:<28} {detail}");
            }
            if o.is_clean() {
                println!(
                    "{GREEN}✓ all {} branches integrated clean{RESET} on `{}`.\n  Push it: git -C {} push -u origin HEAD:{}",
                    o.results.len(),
                    o.integration_branch,
                    wt_str,
                    o.integration_branch,
                );
                // Keep the worktree so the operator can push/inspect.
            } else {
                println!(
                    "{YELLOW}⚠ blocked: {}{RESET} — not landable as one branch; resolve or split.",
                    o.blocked_branches().join(", ")
                );
                let _ = run_git(&root, &["worktree", "remove", "--force", &wt_str]);
            }
        }
        Err(e) => {
            eprintln!("{RED}integration failed: {e}{RESET}");
            let _ = run_git(&root, &["worktree", "remove", "--force", &wt_str]);
        }
    }
    outcome.map(|_| ())
}

/// Resolve the git repo root containing `start` (`git rev-parse --show-toplevel`).
fn git_repo_root(start: &std::path::Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "not inside a git repo ({}): {}",
            start.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

/// Run a git command in `repo`, erroring with stderr on non-zero exit.
fn run_git(repo: &std::path::Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

async fn print_pm_doctor(pool: &sqlx::PgPool) -> Result<()> {
    const MAX_ASSIGN_PER_TICK: i64 = 64;

    println!("{CYAN}▶ Pillar-4 work_item pipeline doctor{RESET}");

    let fresh_leaders: Vec<String> = sqlx::query_scalar(
        "SELECT member_name FROM fleet_leader_state \
          WHERE heartbeat_at > NOW() - INTERVAL '60 seconds'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor leader check: {e}"))?;

    let status_counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) \
           FROM work_items \
          GROUP BY status \
          ORDER BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor work_items status counts: {e}"))?;

    let active_leases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) \
           FROM work_item_leases \
          WHERE released_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor active leases: {e}"))?;

    let stale_leases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) \
           FROM work_item_leases \
          WHERE released_at IS NULL \
            AND lease_expires_at < NOW()",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor stale leases: {e}"))?;

    let free_slots: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) \
           FROM sub_agents \
          WHERE status = 'idle'",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor free slots: {e}"))?;

    let active_by_project = ff_db::pg_active_lease_counts_by_project(pool)
        .await
        .map_err(|e| anyhow::anyhow!("doctor active work by project: {e}"))?;
    let ready = ff_db::pg_ready_work_items(pool, MAX_ASSIGN_PER_TICK)
        .await
        .map_err(|e| anyhow::anyhow!("doctor ready work by project: {e}"))?;
    let mut project_active: std::collections::BTreeMap<Option<String>, i64> =
        active_by_project.into_iter().collect();
    for item in ready {
        project_active.entry(item.project_id).or_default();
    }

    let active_by_computer: std::collections::HashMap<uuid::Uuid, i64> = sqlx::query_as(
        "SELECT computer_id, COUNT(*)::bigint
               FROM work_item_leases
              WHERE released_at IS NULL
              GROUP BY computer_id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor active work by computer: {e}"))?
    .into_iter()
    .collect();
    let dispatch_fresh: std::collections::HashSet<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM computers
              WHERE dispatch_tick_at > NOW() - INTERVAL '60 seconds'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("doctor dispatch freshness: {e}"))?
    .into_iter()
    .collect();
    let schedulable_free_slots = ff_db::pg_free_slots(pool, None, MAX_ASSIGN_PER_TICK)
        .await
        .map_err(|e| anyhow::anyhow!("doctor schedulable free slots: {e}"))?
        .into_iter()
        .filter(|slot| {
            dispatch_fresh.contains(&slot.computer_id)
                && active_by_computer
                    .get(&slot.computer_id)
                    .copied()
                    .unwrap_or(0)
                    < 3
        })
        .count() as i64;
    let fair_share =
        project_fair_share(project_active.len(), active_leases + schedulable_free_slots);
    let project_active: Vec<_> = project_active.into_iter().collect();

    // Orphaned `in_progress` work_items with NO active lease — invisible to the
    // lease-based reaper, so they sit forever. The scheduler's orphan sweep
    // cancels them after an hour; surface any here so "healthy" isn't a lie.
    let orphaned_in_progress = ff_db::pg_count_orphaned_work_items(pool, 3600)
        .await
        .map_err(|e| anyhow::anyhow!("doctor orphaned work_items: {e}"))?;
    let jira_dispatch: (i64, i64, i64, i64) = sqlx::query_as(jira_dispatch_doctor_sql())
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("doctor Jira dispatch invariants: {e}"))?;

    let leader_ok = !fresh_leaders.is_empty();
    if leader_ok {
        println!("{GREEN}✓ leader fresh{RESET}: {}", fresh_leaders.join(", "));
    } else {
        println!("{YELLOW}⚠ leader fresh{RESET}: no heartbeat in the last 60s");
    }

    let status_detail = if status_counts.is_empty() {
        "no work_items".to_string()
    } else {
        status_counts
            .iter()
            .map(|(status, count)| format!("{status} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("{GREEN}✓ work_items by status{RESET}: {status_detail}");

    println!("{GREEN}✓ active leases{RESET}: {active_leases}");
    print!("{}", render_project_fairness(&project_active, fair_share));

    let stale_ok = stale_leases == 0;
    if stale_ok {
        println!("{GREEN}✓ stale leases{RESET}: 0");
    } else {
        println!("{YELLOW}⚠ stale leases{RESET}: {stale_leases}");
    }

    let slots_ok = free_slots > 0;
    if slots_ok {
        println!("{GREEN}✓ free slots{RESET}: {free_slots}");
    } else {
        println!("{YELLOW}⚠ free slots{RESET}: 0 idle sub_agents");
    }

    let orphans_ok = orphaned_in_progress == 0;
    if orphans_ok {
        println!("{GREEN}✓ orphaned in_progress{RESET}: 0");
    } else {
        println!(
            "{YELLOW}⚠ orphaned in_progress{RESET}: {orphaned_in_progress} (no active lease — the scheduler sweep cancels these hourly)"
        );
    }

    let jira_ok = jira_dispatch == (0, 0, 0, 0);
    print!(
        "{}",
        render_jira_dispatch_diagnostics(
            jira_dispatch.0,
            jira_dispatch.1,
            jira_dispatch.2,
            jira_dispatch.3,
        )
    );

    println!();
    if leader_ok && stale_ok && slots_ok && orphans_ok && jira_ok {
        println!("{GREEN}✓ Summary:{RESET} Pillar-4 work_item pipeline is healthy");
    } else {
        println!("{RED}⚠ Summary:{RESET} Pillar-4 work_item pipeline needs attention");
    }

    Ok(())
}

fn jira_dispatch_doctor_sql() -> &'static str {
    "WITH RECURSIVE jira_lineage AS ( \
         SELECT p.id AS root_id, p.id AS item_id, \
                LOWER(BTRIM(COALESCE(p.metadata->>'jira_status', ''))) AS jira_status, \
                p.metadata->>'jira_held_at' AS jira_held_at \
           FROM work_items p \
          WHERE p.kind = 'jira' \
         UNION ALL \
         SELECT l.root_id, c.id, l.jira_status, l.jira_held_at \
           FROM jira_lineage l \
           JOIN work_items c ON c.parent_id = l.item_id \
     ), blocked_roots AS ( \
         SELECT l.root_id, l.jira_held_at \
           FROM jira_lineage l \
          WHERE l.root_id = l.item_id \
            AND l.jira_status IN ('blocked', 'blocked on vinny') \
     ), scheduler_ready AS ( \
         SELECT l.root_id, l.item_id \
           FROM jira_lineage l \
           JOIN work_items w ON w.id = l.item_id \
          WHERE w.status = 'ready' AND NOT w.parked \
            AND (w.kind = 'jira' OR w.kind = 'task') \
     ) \
     SELECT \
       COUNT(DISTINCT b.root_id) FILTER ( \
           WHERE p.status = 'ready' AND NOT p.parked)::bigint AS blocked_parents_ready, \
       COUNT(DISTINCT l.item_id) FILTER ( \
           WHERE l.item_id <> l.root_id \
             AND c.status IN ('ready', 'claimed', 'building', 'in_progress'))::bigint \
           AS live_children_of_blocked, \
       COUNT(DISTINCT s.item_id) FILTER ( \
           WHERE w.repo_id IS NULL OR NOT EXISTS ( \
               SELECT 1 FROM project_repos pr \
                WHERE pr.id = w.repo_id AND pr.project_id = w.project_id))::bigint \
           AS eligible_without_repo, \
       COUNT(DISTINCT b.root_id) FILTER ( \
           WHERE p.status = 'ready' AND NOT p.parked \
             AND (b.jira_held_at IS NULL \
                  OR b.jira_held_at !~ '^\\d{4}-\\d{2}-\\d{2}T' \
                  OR b.jira_held_at::timestamptz < NOW() - INTERVAL '15 minutes'))::bigint \
           AS stale_eligibility \
       FROM blocked_roots b \
       JOIN work_items p ON p.id = b.root_id \
       LEFT JOIN jira_lineage l ON l.root_id = b.root_id \
       LEFT JOIN work_items c ON c.id = l.item_id \
       FULL JOIN scheduler_ready s ON TRUE \
       LEFT JOIN work_items w ON w.id = s.item_id"
}

fn render_jira_dispatch_diagnostics(
    blocked_parents_ready: i64,
    live_children_of_blocked: i64,
    eligible_without_repo: i64,
    stale_eligibility: i64,
) -> String {
    if (
        blocked_parents_ready,
        live_children_of_blocked,
        eligible_without_repo,
        stale_eligibility,
    ) == (0, 0, 0, 0)
    {
        return format!("{GREEN}✓ Jira dispatch invariants{RESET}: clear\n");
    }
    format!(
        "{RED}✗ Jira dispatch invariants{RESET}: blocked parents ready {blocked_parents_ready}, \
         live children {live_children_of_blocked}, no canonical repo {eligible_without_repo}, \
         stale holds {stale_eligibility}\n"
    )
}

fn project_fair_share(project_count: usize, total_capacity: i64) -> i64 {
    if project_count == 0 {
        return total_capacity;
    }
    let project_count = project_count as i64;
    (total_capacity + project_count - 1) / project_count
}

fn render_project_fairness(project_active: &[(Option<String>, i64)], fair_share: i64) -> String {
    if project_active.is_empty() {
        return format!("{GREEN}✓ project fairness{RESET}: no active or ready projects\n");
    }

    let mut output = format!(
        "{GREEN}✓ project fairness{RESET}:\n  {:<24} {:>11} {:>11}\n",
        "PROJECT", "ACTIVE WORK", "FAIR SHARE"
    );
    for (project, active) in project_active {
        output.push_str(&format!(
            "  {:<24} {:>11} {:>11}\n",
            project.as_deref().unwrap_or("<unassigned>"),
            active,
            fair_share
        ));
    }
    output
}

/// One-line fleet-wide rollup for `ff pm board`: work_items by status, live
/// (un-released, un-expired) lease count, and merge-queue depth. The
/// distributed-concurrent-dev picture before the per-item detail.
async fn print_board_summary(pool: &sqlx::PgPool) -> Result<()> {
    let status_counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM work_items GROUP BY status ORDER BY count(*) DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("board summary (statuses): {e}"))?;
    let total: i64 = status_counts.iter().map(|(_, n)| n).sum();
    let active_leases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_item_leases \
          WHERE released_at IS NULL AND lease_expires_at > NOW()",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("board summary (leases): {e}"))?;
    let mq: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM work_item_merge_queue GROUP BY status ORDER BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("board summary (merge-queue): {e}"))?;
    let mq_total: i64 = mq.iter().map(|(_, n)| n).sum();
    let provenance: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COUNT(*) FILTER (WHERE cleanup_complete)::bigint \
           FROM work_item_provenance",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("board summary (provenance): {e}"))?;

    let mq_detail = if mq.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            mq.iter()
                .map(|(s, n)| format!("{s} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    println!(
        "{CYAN}▶ Fleet PM board — {total} work_items · {GREEN}{active_leases} building{RESET}{CYAN} \
         · merge-queue: {mq_total}{mq_detail}{RESET}"
    );
    println!(
        "\x1b[2m  provenance: {} recorded · {} cleanup verified{RESET}",
        provenance.0, provenance.1
    );
    if !status_counts.is_empty() {
        let statuses = status_counts
            .iter()
            .map(|(s, n)| format!("{s} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        println!("\x1b[2m  status: {statuses}{RESET}");
    }
    println!();
    Ok(())
}

/// The planner→PM bridge: decompose a goal into leaf `task` work_items via a
/// fleet LLM, create each as a child (parent_id), and optionally flag ready so
/// the Pillar-4 scheduler fans them across the fleet. This is what turns "give
/// ff a goal" into a fanned-out fleet build (instead of hand-creating tasks).
async fn handle_pm_decompose(
    pool: &sqlx::PgPool,
    goal: String,
    project: String,
    llm: Option<String>,
    repo: Option<String>,
    cwd: Option<PathBuf>,
    ready: bool,
    max: usize,
) -> Result<()> {
    // If `goal` is a work_item UUID, decompose ITS title+description; else treat
    // the string itself as the goal. When it's an existing item, the children
    // hang off it via parent_id.
    let (parent_id, goal_text, parent_repo, parent_project): (
        Option<uuid::Uuid>,
        String,
        Option<crate::repo_context::RepoContext>,
        Option<String>,
    ) = match uuid::Uuid::parse_str(goal.trim()) {
        Ok(uid) => {
            let row: Option<(
                    String,
                    Option<String>,
                    Option<uuid::Uuid>,
                    Option<String>,
                    Option<String>,
                    String,
                )> =
                    sqlx::query_as(
                        "SELECT title, description, repo_id, repo_url, repo_path, project_id FROM work_items WHERE id = $1",
                    )
                        .bind(uid)
                        .fetch_optional(pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("load work item {uid}: {e}"))?;
            match row {
                Some((title, desc, repo_id, repo_url, repo_path, project_id)) => (
                    Some(uid),
                    format!("{title}\n\n{}", desc.unwrap_or_default()),
                    repo_context_from_binding(repo_id, repo_url, repo_path),
                    Some(project_id),
                ),
                None => return Err(anyhow::anyhow!("no work item with id {goal}")),
            }
        }
        Err(_) => (None, goal.clone(), None, None),
    };

    // Children MUST inherit the PARENT's project — not the CLI's `--project`
    // default (which is the cwd repo, e.g. forge-fleet). 2026-07-28: decomposing
    // hireflow360 jira from a forge-fleet checkout created leaf tasks tagged
    // project_id='forge-fleet' with repo_path=…/forge-fleet, so glm built
    // "Add background check model to ff-db" against the WRONG repo → garbage diff
    // → acceptance-gate reject → requeue, never a PR (the "0 PRs / 0 completions"
    // layer). An explicit `--project` still overrides (unchanged when it differs
    // from the default); a bare decompose of an existing item now follows its
    // parent's project.
    let effective_project = parent_project.clone().unwrap_or_else(|| project.clone());

    let repo_context = if repo.is_none() && cwd.is_none() && parent_repo.is_some() {
        parent_repo
    } else {
        crate::repo_context::resolve_repo_context(pool, &effective_project, cwd, repo.as_deref())
            .await?
    };

    println!("{CYAN}▶ decomposing goal via fleet LLM…{RESET}");
    if let Some(ctx) = &repo_context {
        println!(
            "  target repo: {} ({}, {})",
            ctx.repo_url.as_deref().unwrap_or("unknown"),
            ctx.repo_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "no local path".to_string()),
            ctx.primary_language
        );
    }
    let repo_block = repo_context
        .as_ref()
        .map(|ctx| format!("{}\n", ctx.prompt_block()))
        .unwrap_or_else(|| {
            "Target repository context:\n- unknown; infer cautiously from the goal and do not assume forge-fleet.\n\n".to_string()
        });
    // Give the LLM the repo's REAL source directories so its `files` point at a
    // layout that exists. Without this it invents a plausible-but-wrong tree
    // (observed: `crates/forge-fleet/src/commands/pm.rs`, which doesn't exist —
    // the real path is `crates/ff-terminal/src/pm_cmd.rs`). Directory-level (not
    // a full file dump) keeps it context-frugal. Fail-open: no local repo / not
    // a git tree → empty, prompt behaves as before.
    let tracked_files = repo_context
        .as_ref()
        .and_then(|ctx| ctx.repo_path.as_deref())
        .and_then(git_ls_files);
    let crate_block = tracked_files
        .as_deref()
        .and_then(|files| workspace_crate_hint(files, &goal_text))
        .map(|s| format!("{s}\n"))
        .unwrap_or_default();
    let dir_block = tracked_files
        .as_deref()
        .and_then(|files| source_dir_summary(files, 120))
        .map(|s| format!("{s}\n"))
        .unwrap_or_default();
    let prompt = format!(
        "You are a senior engineer breaking a goal into independent, well-scoped \
         LEAF coding tasks for the target repository. Plan against the target \
         repository context below. Do not use the project's primary repository \
         unless it is the target repository.\n\n\
         GRANULARITY (critical — the agents that build these are small local \
         models that fumble sprawling changes): make each task as SMALL as \
         possible — ideally touching exactly ONE file. If a change spans several \
         files, emit ONE task PER FILE rather than a single multi-file task, and \
         make each self-contained (a per-file task that compiles/passes on its \
         own is far better than one big task that a small model half-finishes).\n\n\
         {repo_block}\
         {crate_block}\
         {dir_block}\
         Output ONLY a JSON array (no prose, no markdown fence) of at most {max} \
         objects, each: {{\"title\": \"<imperative, <70 chars>\", \"description\": \
         \"<precise instructions: which file, what to add/change, mirror an \
         existing pattern; name the correct test/build command for this repo \
         when useful>\", \"files\": [\"<repo-relative path(s) this task edits — \
         prefer exactly one; every path MUST be a file that already exists in \
         the repository (scope new code into existing files)>\"], \
         \"complexity\": \"<mechanical|moderate|complex>\", \
         \"acceptance_criteria\": [\"<2-5 SHORT, SPECIFIC, CHECKABLE statements of \
         what a CORRECT implementation must satisfy — the reviewer/builder verifies \
         the diff against EACH one. Be concrete: name the exact function/symbol, \
         the exact behavior, and that it compiles. e.g. 'greet() has a /// doc \
         comment', 'the comment describes the return value', 'cargo check passes'. \
         Avoid vague criteria like 'works correctly'>\"]}}. \
         `complexity` = mechanical for a one-file localized edit, complex for a \
         cross-cutting change. The acceptance_criteria are the DEFINITION OF DONE — \
         the build is only accepted when the diff satisfies every one. \
         GOAL:\n{goal_text}"
    );

    // Decomposition is a PLANNING task — it benefits from the strongest model,
    // but fleet_oneshot orders candidates `tier ASC` (cheapest first), so with no
    // hint it lands on a weak model that half-follows the instructions and
    // hallucinates paths (observed: veronica emitting a fabricated layout). When
    // the caller didn't pin `--llm`, steer to the strongest tool-calling model
    // that has a healthy deployment (data-driven, no hardcoded names). An
    // explicit `--llm` still wins; a fleet with nothing qualifying falls back to
    // the default router (hint = None).
    let effective_hint: Option<String> = match &llm {
        Some(explicit) => Some(explicit.clone()),
        None => strongest_planner_hint(pool).await,
    };
    if llm.is_none() {
        if let Some(h) = &effective_hint {
            println!("  planner: {h} (strongest tool-calling deployment)");
        }
    }
    let resp = ff_agent::fleet_oneshot::fleet_oneshot(
        pool,
        &prompt,
        effective_hint.as_deref(),
        Some(std::time::Duration::from_secs(180)),
    )
    .await
    .map_err(|e| anyhow::anyhow!("fleet_oneshot decompose: {e}"))?;

    let tasks = parse_leaf_tasks(&resp.text)?;
    let tasks = match quality_gate_decomposition(tasks, repo_context.as_ref()) {
        Ok(tasks) => tasks,
        Err(first_error) => {
            // Bad paths and false assumptions are planning errors, not work for a
            // builder to discover after taking a lease. Give the planner exactly
            // one chance to regenerate with the deterministic findings.
            let retry_prompt = format!(
                "{prompt}\n\nYour previous decomposition failed deterministic repository checks:\n\
                 {first_error}\nRegenerate the complete JSON array. Reference only files that already \
                 exist in the repository (scope new code into existing files), and do not \
                 repeat the same file scope in sibling tasks."
            );
            let retry = ff_agent::fleet_oneshot::fleet_oneshot(
                pool,
                &retry_prompt,
                effective_hint.as_deref(),
                Some(std::time::Duration::from_secs(180)),
            )
            .await
            .map_err(|e| anyhow::anyhow!("fleet_oneshot decompose regeneration: {e}"))?;
            let retry_tasks = parse_leaf_tasks(&retry.text)?;
            quality_gate_decomposition(retry_tasks, repo_context.as_ref()).map_err(|e| {
                anyhow::anyhow!("decomposition quality gate failed after regeneration: {e}")
            })?
        }
    };

    let created_by = ff_agent::fleet_info::resolve_this_worker_name().await;
    println!(
        "{GREEN}✓ {} leaf task(s) from {}{RESET}",
        tasks.len(),
        resp.worker_name
    );
    let mut ids = Vec::new();
    for t in tasks.into_iter().take(max) {
        let complexity = normalize_complexity(t.complexity.as_deref());
        let row = insert_decomposed_work_item(
            pool,
            &effective_project,
            &t.title,
            &t.description,
            &created_by,
            parent_id,
            repo_context.as_ref(),
            &t.files,
            complexity,
            &t.acceptance_criteria,
        )
        .await?;
        ids.push(row.0);
        let scope = match t.files.len() {
            0 => "unscoped".to_string(),
            1 => t.files[0].clone(),
            n => format!("{n} files"),
        };
        println!("  + {} [{complexity}] {} ({scope})", row.0, t.title);
    }

    if ready {
        for id in &ids {
            sqlx::query("UPDATE work_items SET status = 'ready' WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await
                .map_err(|e| anyhow::anyhow!("flag ready {id}: {e}"))?;
        }
        if let Some(parent_id) = parent_id {
            sqlx::query(
                "UPDATE work_items SET status = 'decomposed', last_error = NULL \
                 WHERE id = $1 AND status IN ('idea', 'ready', 'decomposing')",
            )
            .bind(parent_id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("mark parent {parent_id} decomposed: {e}"))?;
        }
        println!(
            "{GREEN}✓ flagged {} task(s) ready — the Pillar-4 scheduler will fan them across the fleet{RESET}",
            ids.len()
        );
    } else {
        println!(
            "{YELLOW}note:{RESET} created as 'idea' — re-run with --ready (or `ff pm ready <id>`) to dispatch"
        );
    }
    Ok(())
}

fn parse_leaf_tasks(text: &str) -> Result<Vec<LeafTask>> {
    let text = text.trim();
    let json_slice = match (text.find('['), text.rfind(']')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => return Err(anyhow::anyhow!("LLM did not return a JSON array")),
    };
    let tasks: Vec<LeafTask> = serde_json::from_str(json_slice)
        .map_err(|e| anyhow::anyhow!("parse decomposed tasks JSON: {e}"))?;
    if tasks.is_empty() {
        return Err(anyhow::anyhow!("LLM returned zero tasks"));
    }
    Ok(tasks)
}

fn quality_gate_decomposition(
    tasks: Vec<LeafTask>,
    repo_context: Option<&crate::repo_context::RepoContext>,
) -> Result<Vec<LeafTask>> {
    let tracked = repo_context
        .and_then(|ctx| ctx.repo_path.as_deref())
        .and_then(git_ls_files)
        .map(|files| {
            files
                .lines()
                .map(str::to_owned)
                .collect::<std::collections::HashSet<_>>()
        });
    // FAIL-CLOSED (operator 2026-07-25): an UNGROUNDED decomposition (no local
    // repo to validate against) produced 64 backlog items with hallucinated
    // Python paths in a Rust repo — items that can NEVER build and churned the
    // pipeline. Require a resolved repo for EVERY decomposition (previously only
    // `autonomous` ones), so predicted paths are ALWAYS checked against real
    // tracked files. Better to refuse an ungrounded decompose loudly than to
    // emit unbuildable garbage.
    if tracked.is_none() {
        return Err(anyhow::anyhow!(
            "decomposition quality gate: no local git repo resolved for path validation — \
             refusing an ungrounded decomposition (the source of the mis-decomposed backlog). \
             Resolve the project's repo checkout before decomposing."
        ));
    }

    let repo_path = repo_context.and_then(|ctx| ctx.repo_path.as_deref());

    for task in &tasks {
        if task.files.is_empty() {
            return Err(anyhow::anyhow!(
                "decomposition quality gate: '{}' has no file references",
                task.title
            ));
        }
        for file in &task.files {
            // A referenced file must either ALREADY exist, OR be a legitimate NEW
            // file whose PARENT DIRECTORY exists in the repo (operator 2026-07-25:
            // ff must be able to build NEW features = new modules, e.g.
            // `crates/ff-core/src/slots.rs` in the existing `crates/ff-core/src`).
            // A path whose parent dir does NOT exist is a cross-language / wrong-
            // structure hallucination (e.g. `mcp/service.py`, `src/usage_pollers/`
            // in a Rust repo) and is still rejected — this is what kept the
            // polluted backlog out while unblocking real feature work.
            let parent_dir_exists = repo_path
                .map(|rp| {
                    std::path::Path::new(file)
                        .parent()
                        .map(|p| p.as_os_str().is_empty() || rp.join(p).is_dir())
                        .unwrap_or(true)
                })
                .unwrap_or(false);

            if let Some(tracked) = &tracked
                && !tracked.contains(file)
                && !parent_dir_exists
            {
                return Err(anyhow::anyhow!(
                    "decomposition quality gate: '{}' references '{}' — neither an existing tracked file nor a new file in an existing directory (likely a wrong-structure hallucination)",
                    task.title,
                    file
                ));
            }
            // Existing-but-vanished (git ls-files still lists a deleted file) is a
            // hard error; a new file whose parent dir exists is allowed.
            if let Some(repo_path) = repo_path
                && !find_file_exists(repo_path, file)
                && !parent_dir_exists
            {
                return Err(anyhow::anyhow!(
                    "decomposition quality gate: '{}' references file '{}' that does not exist in the repository",
                    task.title,
                    file
                ));
            }
        }
        if let Some(repo_path) = repo_path {
            verify_symbol_claims(task, repo_path)?;
        }
    }
    Ok(merge_overlapping_siblings(tasks))
}

/// Collapse siblings whose predicted file sets intersect. Keeping the first
/// title makes output stable; combining descriptions preserves both requested
/// changes while ensuring two workers cannot race on the same file.
fn merge_overlapping_siblings(tasks: Vec<LeafTask>) -> Vec<LeafTask> {
    let mut merged: Vec<LeafTask> = Vec::new();
    for mut task in tasks {
        // Remove and fold every intersection, not just the first: a task that
        // bridges two otherwise-disjoint scopes must collapse all three.
        let mut index = 0;
        let mut retained_title = None;
        while index < merged.len() {
            if merged[index]
                .files
                .iter()
                .any(|file| task.files.contains(file))
            {
                let existing = merged.remove(index);
                if retained_title.is_none() {
                    retained_title = Some(existing.title.clone());
                }
                if !task.description.contains(&existing.description) {
                    task.description =
                        format!("{}\n\nAlso: {}", existing.description, task.description);
                }
                for file in existing.files {
                    if !task.files.contains(&file) {
                        task.files.push(file);
                    }
                }
                task.complexity = Some(merge_complexity(
                    existing.complexity.as_deref(),
                    task.complexity.as_deref(),
                ));
            } else {
                index += 1;
            }
        }
        if let Some(title) = retained_title {
            task.title = title;
        }
        merged.push(task);
    }
    merged
}

fn merge_complexity(left: Option<&str>, right: Option<&str>) -> String {
    let rank = |value: Option<&str>| match normalize_complexity(value) {
        "complex" => 2,
        "moderate" => 1,
        _ => 0,
    };
    match rank(left).max(rank(right)) {
        2 => "complex",
        1 => "moderate",
        _ => "mechanical",
    }
    .to_string()
}

/// Check explicit existence/absence claims around backticked Rust-like symbols.
/// This deliberately ignores ordinary mentions: only sentences containing a
/// claim word are checked, keeping the deterministic grep conservative.
fn verify_symbol_claims(task: &LeafTask, repo_path: &Path) -> Result<()> {
    for sentence in task.description.split(['.', '\n']) {
        let lower = sentence.to_ascii_lowercase();
        let claims_absent = lower.contains("does not exist")
            || lower.contains("doesn't exist")
            || lower.contains("is missing")
            || lower.contains("is absent");
        let claims_present = lower.contains("existing")
            || lower.contains("already exists")
            || lower.contains("is present");
        if !claims_absent && !claims_present {
            continue;
        }
        for symbol in backticked_symbols(sentence) {
            let exists = git_grep_symbol(repo_path, symbol);
            if (claims_absent && exists) || (claims_present && !exists) {
                return Err(anyhow::anyhow!(
                    "decomposition quality gate: '{}' makes a false {} claim about symbol `{}`",
                    task.title,
                    if claims_absent {
                        "absence"
                    } else {
                        "existence"
                    },
                    symbol
                ));
            }
        }
    }
    Ok(())
}

fn backticked_symbols(text: &str) -> Vec<&str> {
    let mut parts = text.split('`');
    let mut symbols = Vec::new();
    while let Some(_) = parts.next() {
        let Some(candidate) = parts.next() else { break };
        if !candidate.is_empty()
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':'))
            && candidate.chars().any(|c| c.is_ascii_alphabetic())
        {
            symbols.push(candidate);
        }
    }
    symbols
}

fn git_grep_symbol(repo_path: &Path, symbol: &str) -> bool {
    let grep = |pattern: &str| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["grep", "-q", "-F", "--", pattern])
            .status()
            .is_ok_and(|status| status.success())
    };
    grep(symbol) || symbol.rsplit_once("::").is_some_and(|(_, leaf)| grep(leaf))
}

fn repo_context_from_binding(
    repo_id: Option<uuid::Uuid>,
    repo_url: Option<String>,
    repo_path: Option<String>,
) -> Option<crate::repo_context::RepoContext> {
    if repo_id.is_none() && repo_url.is_none() && repo_path.is_none() {
        return None;
    }
    let repo_path = repo_path.map(PathBuf::from);
    let mut ctx = repo_path
        .as_deref()
        .and_then(|p| crate::repo_context::detect_repo_from_cwd(p).ok())
        .unwrap_or_else(|| crate::repo_context::RepoContext {
            repo_id: None,
            repo_url: None,
            repo_path: repo_path.clone(),
            primary_language: "unknown".to_string(),
            build_system: None,
            key_dirs: Vec::new(),
        });
    ctx.repo_id = repo_id;
    if repo_url.is_some() {
        ctx.repo_url = repo_url;
    }
    if repo_path.is_some() {
        ctx.repo_path = repo_path;
    }
    Some(ctx)
}

/// The strongest tool-calling model with a healthy deployment, as a routing
/// hint for the decompose PLANNING call. `fleet_oneshot` orders `tier ASC`
/// (cheapest first) — great for cheap one-shots, wrong for planning, which wants
/// the LARGEST capable model. We pick the highest-tier `tool_calling` model that
/// currently has a healthy deployment (tool-calling excludes the weak non-agent
/// models like gemma; tier DESC = biggest first). Passed as `model_hint`, which
/// `fleet_oneshot` prefers then fails over from. Data-driven (no hardcoded
/// names); `None` when nothing qualifies → caller keeps default routing.
async fn strongest_planner_hint(pool: &sqlx::PgPool) -> Option<String> {
    // Pick the strongest tool-calling model that is ACTUALLY SERVING right now —
    // healthy AND fresh (health probed in the last 180s), the same routability bar
    // the router uses. Without the freshness gate this returned a STALE 'healthy'
    // row for a dead endpoint (Qwen3-Coder-30B-A3B lingered 'healthy' after it
    // stopped serving), so decompose pinned that hint and fleet_oneshot then found
    // "no healthy fleet deployment" — the feeder silently failed to decompose ANY
    // idea for days (operator 2026-07-26). With the gate it picks glm-4.5-air
    // (tool-calling, healthy+fresh on ~10 nodes) instead. Go through the router's
    // real live state, never a name that isn't servable.
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT cat.name \
           FROM fleet_model_deployments d \
           JOIN fleet_model_catalog cat ON cat.id = d.catalog_id \
          WHERE d.desired_state = 'active' \
            AND d.health_status = 'healthy' \
            AND EXTRACT(EPOCH FROM (now() - d.last_health_at)) <= 180 \
            AND cat.tool_calling = TRUE \
            AND cat.name IS NOT NULL AND cat.name <> '' \
          ORDER BY cat.tier DESC, d.last_health_at DESC NULLS LAST \
          LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    row.map(|(name,)| name)
}

/// Verify a referenced path exists in the working tree via `find . -name`.
/// The basename is the `-name` pattern, but a hit only counts when one of the
/// printed paths matches the referenced path exactly — a same-named file in
/// another directory cannot vouch for a hallucinated one. Fail-closed: a
/// missing basename or a `find` that cannot run means the file does not exist.
fn find_file_exists(repo_path: &Path, file: &str) -> bool {
    let Some(name) = Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        return false;
    };
    let Ok(out) = std::process::Command::new("find")
        .arg(".")
        .arg("-name")
        .arg(&name)
        .current_dir(repo_path)
        .output()
    else {
        return false;
    };
    // Match on stdout even for a nonzero exit: `find` returns failure on any
    // unreadable subdirectory while still printing every valid hit.
    let target = format!("./{}", file.trim_start_matches("./"));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line == target)
}

/// Best-effort `git ls-files` for the target repo (newline-joined). Returns
/// None when there's no local path or it isn't a git tree — fail-open, the
/// caller just omits the directory hint.
fn git_ls_files(repo_path: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("ls-files")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Summarize a `git ls-files` listing into the repo's real SOURCE directories
/// (deduped, sorted, with a per-dir file count), so the decompose LLM anchors
/// its predicted paths to a layout that EXISTS instead of inventing one.
/// Directory-level (not a full file dump) keeps the prompt context-frugal.
/// Returns None if no source files are present.
fn source_dir_summary(ls_files: &str, max_dirs: usize) -> Option<String> {
    use std::collections::BTreeMap;
    const SRC_EXT: &[&str] = &[
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".rb", ".sh", ".toml", ".sql",
    ];
    let mut dirs: BTreeMap<&str, usize> = BTreeMap::new();
    for line in ls_files.lines() {
        if line.contains("/target/") || line.contains("node_modules/") {
            continue;
        }
        if !SRC_EXT.iter().any(|e| line.ends_with(e)) {
            continue;
        }
        let dir = line.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        *dirs.entry(dir).or_default() += 1;
    }
    if dirs.is_empty() {
        return None;
    }
    let total = dirs.len();
    let mut s = String::from(
        "Real source directories in the target repo (predicted `files` MUST live \
         under one of these — do NOT invent a layout):\n",
    );
    for (i, (dir, count)) in dirs.iter().enumerate() {
        if i >= max_dirs {
            s.push_str(&format!("- … ({} more directories)\n", total - max_dirs));
            break;
        }
        s.push_str(&format!("- {dir}/ ({count} files)\n"));
    }
    Some(s)
}

/// Point the planner at a workspace crate named in the goal. Without an
/// explicit root, models commonly treat a crate such as `ff-agent` as the
/// repository root and emit `src/lib.rs` instead of
/// `crates/ff-agent/src/lib.rs`.
fn workspace_crate_hint(ls_files: &str, goal: &str) -> Option<String> {
    let goal = goal.to_ascii_lowercase();
    let mut crates = std::collections::BTreeSet::new();
    for file in ls_files.lines() {
        let Some(rest) = file.strip_prefix("crates/") else {
            continue;
        };
        let Some((crate_name, _)) = rest.split_once('/') else {
            continue;
        };
        // Goals often name the crate the way Rust code actually references it
        // (`ff_core::...`, underscored) rather than its Cargo package/directory
        // name (`ff-core`, hyphenated). Match either spelling so the hint still
        // fires — otherwise a goal like "fix ff_core task generation" misses
        // the hint and the planner emits bare `src/...` paths.
        let underscored = crate_name.replace('-', "_").to_ascii_lowercase();
        if goal.contains(&crate_name.to_ascii_lowercase()) || goal.contains(&underscored) {
            crates.insert(crate_name);
        }
    }
    if crates.is_empty() {
        return None;
    }
    let roots = crates
        .into_iter()
        .map(|name| format!("`{name}` => `crates/{name}/`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Workspace crate roots named in the goal: {roots}. Keep these full \
         repo-relative prefixes in every `files` path; never shorten them to `src/...`."
    ))
}

/// Normalize an LLM-supplied complexity into the work_items vocabulary
/// (`mechanical` | `moderate` | `complex`). Unknown/absent → `mechanical`
/// (matches the column default; the safe assumption for a well-scoped leaf).
fn normalize_complexity(raw: Option<&str>) -> &'static str {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("complex") => "complex",
        Some("moderate") => "moderate",
        _ => "mechanical",
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_decomposed_work_item(
    pool: &sqlx::PgPool,
    project: &str,
    title: &str,
    description: &str,
    created_by: &str,
    parent_id: Option<uuid::Uuid>,
    repo_context: Option<&crate::repo_context::RepoContext>,
    predicted_paths: &[String],
    complexity: &str,
    acceptance_criteria: &[String],
) -> Result<(uuid::Uuid,)> {
    // NULL when empty so an un-graded task is distinguishable from "0 criteria".
    let criteria_json = if acceptance_criteria.is_empty() {
        None
    } else {
        Some(serde_json::json!(acceptance_criteria))
    };
    sqlx::query_as(decomposed_work_item_insert_sql())
        .bind(project)
        .bind(title)
        .bind(description)
        .bind(created_by)
        .bind(parent_id)
        .bind(repo_context.and_then(|ctx| ctx.repo_id))
        .bind(repo_context.and_then(|ctx| ctx.repo_url.as_deref()))
        .bind(
            repo_context
                .and_then(|ctx| ctx.repo_path.as_ref())
                .map(|p| p.to_string_lossy().to_string()),
        )
        .bind(serde_json::json!(predicted_paths))
        .bind(complexity)
        .bind(criteria_json)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("insert child task: {e}"))
}

fn decomposed_work_item_insert_sql() -> &'static str {
    "INSERT INTO work_items \
        (project_id, kind, title, description, priority, created_by, parent_id, repo_id, repo_url, repo_path, predicted_paths, complexity, acceptance_criteria, context, pre_work, work, post_work) \
     VALUES ($1, 'task', $2, $3, 'normal', $4, $5, $6, $7, $8, $9, $10, $11, \
        COALESCE((SELECT context FROM work_items WHERE id = $5), '{}'::jsonb), \
        COALESCE((SELECT pre_work FROM work_items WHERE id = $5), '[]'::jsonb), \
        COALESCE((SELECT work FROM work_items WHERE id = $5), '[]'::jsonb), \
        COALESCE((SELECT post_work FROM work_items WHERE id = $5), '[]'::jsonb)) RETURNING id"
}

/// A single task as parsed from a Claude Code session transcript.
#[derive(Debug, Default, Clone)]
struct ClaudeTask {
    id: String,
    subject: String,
    description: String,
    status: String,
}

/// Extract content blocks from a JSONL record. Claude Code lines sometimes put
/// the content array under `message.content` and sometimes at the root.
fn extract_content_blocks(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
        if let Some(arr) = content.as_array() {
            return arr.iter().collect();
        }
    }
    if let Some(content) = value.get("content") {
        if let Some(arr) = content.as_array() {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

/// Convert a Claude task status into the `work_items` vocabulary.
fn claude_status_to_work_item(status: &str) -> &'static str {
    match status {
        "pending" => "backlog",
        "in_progress" => "in_progress",
        "completed" => "done",
        "deleted" | "cancelled" => "cancelled",
        _ => "backlog",
    }
}

/// Parse the classic `#1 [pending] subject` text rendering (used by TodoWrite
/// and by TaskList text results). Returns (id, status, subject) triples.
fn parse_task_text_lines(text: &str) -> Vec<(String, String, String)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"#(\d+)\s*(?:\.\s*)?\[(pending|in_progress|completed|deleted|cancelled)\]\s+(.+)",
        )
        .unwrap()
    });
    let mut out = Vec::new();
    for cap in RE.captures_iter(text) {
        let id = cap[1].to_string();
        let status = cap[2].to_string();
        let mut subject = cap[3].trim().to_string();
        // Subject may carry trailing render noise (JSON escapes, dependency
        // annotations, newlines). Trim at the first known terminator.
        for term in ["\\n", "\"", "\n", "[blocked by", " blocked by"] {
            if let Some(pos) = subject.find(term) {
                subject.truncate(pos);
            }
        }
        let subject = subject.trim_end().to_string();
        if !subject.is_empty() {
            out.push((id, status, subject));
        }
    }
    out
}

/// Render a tool_result `content` value as a string. Content may be a plain
/// string, an array of text blocks, or a JSON value.
fn tool_result_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.to_string(),
    }
}

/// Try to pull a task id/subject/description/status out of a JSON task object.
/// Handles both the legacy `subject`/`content` fields and the newer `title` field.
fn task_from_json(value: &serde_json::Value) -> Option<ClaudeTask> {
    let id = value.get("id").and_then(|v| v.as_str())?.to_string();
    let subject = value
        .get("subject")
        .or_else(|| value.get("title"))
        .or_else(|| value.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if subject.is_empty() {
        return None;
    }
    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending")
        .to_string();
    Some(ClaudeTask {
        id,
        subject,
        description,
        status,
    })
}

/// Extract the assigned task id from a TaskCreate tool_result. Claude Code has
/// returned several shapes, so we try a few common ones before falling back to
/// the synthetic tool_use id.
fn task_id_from_create_result(value: &serde_json::Value) -> Option<String> {
    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    if let Some(id) = value
        .get("task")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    // TaskCreate may return a wrapper object with a `tasks` array, e.g.
    // `{ "tasks": [{ "id": "1", ... }] }`.
    if let Some(id) = value
        .get("tasks")
        .and_then(|v| v.as_array())
        .and_then(|v| v.first())
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    let text = tool_result_text(value);
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:[Tt]ask #?|#)(\d+)(?:\s+created:?\s+|\s*\[)?").unwrap()
    });
    RE.captures(&text).map(|cap| cap[1].to_string())
}

/// Parse the current Claude Code task format from a session JSONL transcript.
/// Handles TodoWrite, TaskCreate, TaskUpdate, TaskList, and TaskGet tool
/// invocations, plus the legacy `#N [status] subject` text rendering. Task
/// objects may use `subject` or `title` for the summary, and TaskCreate results
/// may be returned as a single object or a `{ "tasks": [...] }` wrapper.
fn parse_claude_tasks(content: &str) -> std::collections::BTreeMap<String, ClaudeTask> {
    use std::collections::{BTreeMap, HashMap};

    let mut tasks: BTreeMap<String, ClaudeTask> = BTreeMap::new();
    let mut pending_creations: HashMap<String, ClaudeTask> = HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // Assistant side: tool_use inputs.
        for block in extract_content_blocks(&obj) {
            let Some(name) = block.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let input = block.get("input").unwrap_or(&serde_json::Value::Null);
            let tool_use_id = block
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            match name {
                "TodoWrite" => {
                    if let Some(todos) = input.get("todos").and_then(|v| v.as_array()) {
                        tasks.clear();
                        for (idx, todo) in todos.iter().enumerate() {
                            let id = (idx + 1).to_string();
                            let subject = todo
                                .get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let status = todo
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("pending")
                                .to_string();
                            if !subject.is_empty() {
                                tasks.insert(
                                    id.clone(),
                                    ClaudeTask {
                                        id,
                                        subject,
                                        description: String::new(),
                                        status,
                                    },
                                );
                            }
                        }
                    }
                }
                "TaskCreate" => {
                    let subject = input
                        .get("subject")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if subject.is_empty() {
                        continue;
                    }
                    let description = input
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    pending_creations.insert(
                        tool_use_id.clone(),
                        ClaudeTask {
                            id: tool_use_id.clone(),
                            subject,
                            description,
                            status: "pending".to_string(),
                        },
                    );
                }
                "TaskUpdate" => {
                    let task_id = input
                        .get("taskId")
                        .or_else(|| input.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if task_id.is_empty() {
                        continue;
                    }
                    let entry = tasks.entry(task_id.clone()).or_insert_with(|| ClaudeTask {
                        id: task_id.clone(),
                        ..Default::default()
                    });
                    if let Some(status) = input.get("status").and_then(|v| v.as_str()) {
                        entry.status = status.to_string();
                    }
                    if let Some(subject) = input.get("subject").and_then(|v| v.as_str()) {
                        entry.subject = subject.to_string();
                    }
                    if let Some(description) = input.get("description").and_then(|v| v.as_str()) {
                        entry.description = description.to_string();
                    }
                }
                _ => {}
            }
        }

        // Also scan plain text blocks (assistant/user messages) for rendered task lists.
        for block in extract_content_blocks(&obj) {
            if block.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
                continue;
            };
            for (id, status, subject) in parse_task_text_lines(text) {
                let entry = tasks.entry(id.clone()).or_insert_with(|| ClaudeTask {
                    id: id.clone(),
                    ..Default::default()
                });
                entry.subject = subject;
                entry.status = status;
            }
        }

        // User side: tool_result outputs.
        for block in extract_content_blocks(&obj) {
            let Some(tool_use_id) = block.get("tool_use_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let content_val = block.get("content").unwrap_or(&serde_json::Value::Null);

            // Resolve a pending TaskCreate to its real id. If the result contains
            // a `tasks` array we use that authoritative list; otherwise we pair the
            // pending subject/description with the real id extracted from the result.
            let resolved_create = if let Some(pending) = pending_creations.remove(tool_use_id) {
                if let Some(task_arr) = content_val.get("tasks").and_then(|v| v.as_array()) {
                    for task_json in task_arr {
                        if let Some(task) = task_from_json(task_json) {
                            tasks.insert(task.id.clone(), task);
                        }
                    }
                    true
                } else {
                    let real_id = task_id_from_create_result(content_val)
                        .unwrap_or_else(|| tool_use_id.to_string());
                    tasks.insert(
                        real_id.clone(),
                        ClaudeTask {
                            id: real_id.clone(),
                            subject: pending.subject,
                            description: pending.description,
                            status: "pending".to_string(),
                        },
                    );
                    true
                }
            } else {
                false
            };

            if resolved_create {
                continue;
            }

            let text = tool_result_text(content_val);

            // Try structured JSON first (TaskList returns an array; TaskGet an object;
            // TaskCreate sometimes returns a wrapper object with a `tasks` array).
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                for task_json in arr {
                    if let Some(task) = task_from_json(&task_json) {
                        tasks.insert(task.id.clone(), task);
                    }
                }
            } else if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&text) {
                // Some TaskCreate results wrap the task list in { "tasks": [...] }.
                if let Some(task_arr) = obj.get("tasks").and_then(|v| v.as_array()) {
                    for task_json in task_arr {
                        if let Some(task) = task_from_json(task_json) {
                            tasks.insert(task.id.clone(), task);
                        }
                    }
                } else if let Some(task) = task_from_json(&obj) {
                    tasks.insert(task.id.clone(), task);
                }
            }

            // Text fallback inside the tool result.
            for (id, status, subject) in parse_task_text_lines(&text) {
                let entry = tasks.entry(id.clone()).or_insert_with(|| ClaudeTask {
                    id: id.clone(),
                    ..Default::default()
                });
                entry.subject = subject;
                entry.status = status;
            }
        }
    }

    // Any TaskCreate that never got a parseable result is still useful; keep it
    // keyed by its tool_use id so it doesn't disappear.
    for (tool_use_id, pending) in pending_creations {
        tasks.insert(tool_use_id, pending);
    }

    // Final fallback: scan the entire file text for legacy rendered task lists.
    for (id, status, subject) in parse_task_text_lines(content) {
        let entry = tasks.entry(id.clone()).or_insert_with(|| ClaudeTask {
            id: id.clone(),
            ..Default::default()
        });
        if entry.subject.is_empty() {
            entry.subject = subject;
        }
        if entry.status.is_empty() {
            entry.status = status;
        }
    }

    tasks
}
/// Encode a project cwd the way Claude Code names its
/// `~/.claude/projects/<slug>` dir: EVERY non-alphanumeric char becomes `-`
/// (so `/home/x/.forgefleet` → `-home-x--forgefleet`). The old
/// `replace('/', "-")` missed dots and produced `-.forgefleet`-style slugs
/// that match no real dir.
fn claude_project_slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
pub async fn handle_pm_import_claude_tasks(
    pool: &sqlx::PgPool,
    session: Option<PathBuf>,
    project: &str,
    dry_run: bool,
) -> Result<()> {
    // Resolve session path. If the operator didn't pass --session, try
    // to find the most recently-modified .jsonl in the current project's
    // Claude dir. Encoding mirrors Claude's slug: `/Users/venkat/...` →
    // `-Users-venkat-...`.
    let resolved = if let Some(p) = session {
        p
    } else {
        let cwd = std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd: {e}"))?;
        let slug = claude_project_slug(&cwd.to_string_lossy());
        let home = std::env::var("HOME").unwrap_or_default();
        let project_dir = PathBuf::from(format!("{home}/.claude/projects/{slug}"));
        let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
        if let Ok(mut entries) = tokio::fs::read_dir(&project_dir).await {
            while let Some(e) = entries.next_entry().await.unwrap_or(None) {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(md) = e.metadata().await
                    && let Ok(mtime) = md.modified()
                    && newest.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true)
                {
                    newest = Some((path, mtime));
                }
            }
        }
        newest
            .map(|(p, _)| p)
            .ok_or_else(|| anyhow::anyhow!("no session JSONL found under {project_dir:?}"))?
    };

    println!(
        "{CYAN}▶ Importing Claude tasks from{RESET} {}",
        resolved.display()
    );

    // Stream the JSONL and parse the current Claude Code task format.
    // Tasks may be encoded as TodoWrite/TaskCreate/TaskUpdate tool_use inputs,
    // TaskList/TaskGet tool_result outputs, or the legacy rendered text lines.
    let content = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {e}", resolved.display()))?;
    let snapshot = parse_claude_tasks(&content);

    if snapshot.is_empty() {
        println!("  (no task lines recognized in transcript)");
        return Ok(());
    }

    println!("  found {} unique tasks", snapshot.len());
    // Confirm project exists.
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1)")
            .bind(project)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("project probe: {e}"))?;
    if !project_exists {
        eprintln!(
            "{YELLOW}project '{project}' not found — create it first or pass --project X{RESET}"
        );
        std::process::exit(2);
    }

    if dry_run {
        println!("\n{YELLOW}Dry run — not writing.{RESET}");
        for (id, task) in &snapshot {
            let clip = if task.subject.chars().count() > 60 {
                format!("{}…", task.subject.chars().take(59).collect::<String>())
            } else {
                task.subject.clone()
            };
            println!("  would upsert #{id:<3} [{:<11}] {clip}", task.status);
        }
        return Ok(());
    }

    let mut inserted = 0usize;
    let mut updated = 0usize;
    for (id, task) in &snapshot {
        // A TaskUpdate-only trail (task created in an earlier session) can
        // leave the subject unknown — nothing meaningful to import.
        if task.subject.is_empty() {
            continue;
        }
        // Upsert by (project_id, claude_task_id). work_items has no
        // unique constraint on that pair so we check-then-insert/update.
        let existing: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM work_items
              WHERE project_id = $1
                AND metadata->>'claude_task_id' = $2
              LIMIT 1",
        )
        .bind(project)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("lookup existing: {e}"))?;

        let wi_status = claude_status_to_work_item(&task.status);

        if let Some(wi_id) = existing {
            sqlx::query(
                "UPDATE work_items
                    SET status = $1,
                        title  = $2,
                        description = $3
                  WHERE id = $4",
            )
            .bind(wi_status)
            .bind(&task.subject)
            .bind(&task.description)
            .bind(wi_id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("update work_item: {e}"))?;
            updated += 1;
        } else {
            sqlx::query(
                "INSERT INTO work_items
                    (project_id, kind, title, description, status, priority, created_by, metadata)
                 VALUES ($1, 'code', $2, $3, $4, 'normal', 'claude_code',
                         jsonb_build_object('claude_task_id', $5::text,
                                            'imported_at', NOW()::text))",
            )
            .bind(project)
            .bind(&task.subject)
            .bind(&task.description)
            .bind(wi_status)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert work_item: {e}"))?;
            inserted += 1;
        }
    }

    println!(
        "{GREEN}✓ imported{RESET}: {inserted} new, {updated} updated ({} total from Claude)",
        snapshot.len()
    );
    Ok(())
}

/// A `label → count` bucket in a [`PmStats`] rollup (status / kind / project).
#[derive(Debug, Clone, serde::Serialize)]
struct LabeledCount {
    label: String,
    count: i64,
}

/// Backlog + throughput snapshot for `ff pm stats`. Plain data so the render is
/// a pure function (unit-tested) and `--json` is a trivial serialization.
#[derive(Debug, Clone, serde::Serialize)]
struct PmStats {
    total: i64,
    by_status: Vec<LabeledCount>,
    by_kind: Vec<LabeledCount>,
    by_project: Vec<LabeledCount>,
    created_24h: i64,
    created_7d: i64,
    completed_24h: i64,
    completed_7d: i64,
    failed_total: i64,
    /// Age of the oldest `status='ready'` item (still waiting for a slot), or
    /// `None` when nothing is ready.
    oldest_ready_age_secs: Option<i64>,
}

/// Run the rollup queries against live Postgres. NULL `kind`/`project_id` fold
/// into `(none)` so every item is accounted for.
async fn collect_pm_stats(pool: &sqlx::PgPool) -> Result<PmStats> {
    let buckets = |sql: &str| {
        let sql = sql.to_string();
        async move {
            let rows: Vec<(String, i64)> = sqlx::query_as(&sql)
                .fetch_all(pool)
                .await
                .map_err(|e| anyhow::anyhow!("pm stats query: {e}"))?;
            Ok::<_, anyhow::Error>(
                rows.into_iter()
                    .map(|(label, count)| LabeledCount { label, count })
                    .collect::<Vec<_>>(),
            )
        }
    };

    let by_status = buckets(
        "SELECT status, count(*) FROM work_items GROUP BY status ORDER BY count(*) DESC, status",
    )
    .await?;
    let by_kind = buckets(
        "SELECT COALESCE(kind, '(none)'), count(*) FROM work_items \
         GROUP BY kind ORDER BY count(*) DESC, 1",
    )
    .await?;
    let by_project = buckets(
        "SELECT COALESCE(project_id, '(none)'), count(*) FROM work_items \
         GROUP BY project_id ORDER BY count(*) DESC, 1 LIMIT 12",
    )
    .await?;

    let scalar = |sql: &str| {
        let sql = sql.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(&sql)
                .fetch_one(pool)
                .await
                .map_err(|e| anyhow::anyhow!("pm stats scalar: {e}"))
        }
    };
    let total = scalar("SELECT count(*) FROM work_items").await?;
    let created_24h =
        scalar("SELECT count(*) FROM work_items WHERE created_at > NOW() - INTERVAL '24 hours'")
            .await?;
    let created_7d =
        scalar("SELECT count(*) FROM work_items WHERE created_at > NOW() - INTERVAL '7 days'")
            .await?;
    let completed_24h =
        scalar("SELECT count(*) FROM work_items WHERE completed_at > NOW() - INTERVAL '24 hours'")
            .await?;
    let completed_7d =
        scalar("SELECT count(*) FROM work_items WHERE completed_at > NOW() - INTERVAL '7 days'")
            .await?;
    let failed_total = scalar("SELECT count(*) FROM work_items WHERE status = 'failed'").await?;

    let oldest_ready_age_secs: Option<i64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (NOW() - MIN(created_at)))::bigint \
           FROM work_items WHERE status = 'ready'",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("pm stats oldest ready: {e}"))?;

    Ok(PmStats {
        total,
        by_status,
        by_kind,
        by_project,
        created_24h,
        created_7d,
        completed_24h,
        completed_7d,
        failed_total,
        oldest_ready_age_secs,
    })
}

/// Render a coarse human duration (`45m`, `3h`, `5d`) for the oldest-ready age.
/// Pure.
fn humanize_age_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Format a [`PmStats`] as the `ff pm stats` report. Pure (no I/O, no color
/// codes) so it is unit-testable and stable for snapshotting.
fn render_pm_stats(s: &PmStats) -> String {
    let mut out = String::new();
    out.push_str(&format!("work_items — {} rows total\n", s.total));

    let section = |out: &mut String, title: &str, buckets: &[LabeledCount]| {
        out.push_str(&format!("\n  by {title}:\n"));
        if buckets.is_empty() {
            out.push_str("    (none)\n");
            return;
        }
        for b in buckets {
            out.push_str(&format!("    {:<14} {}\n", b.label, b.count));
        }
    };
    section(&mut out, "status", &s.by_status);
    section(&mut out, "kind", &s.by_kind);
    section(&mut out, "project", &s.by_project);

    out.push_str("\n  throughput:\n");
    out.push_str(&format!(
        "    created    {} in 24h · {} in 7d\n",
        s.created_24h, s.created_7d
    ));
    out.push_str(&format!(
        "    completed  {} in 24h · {} in 7d\n",
        s.completed_24h, s.completed_7d
    ));
    out.push_str(&format!("    failed     {} (lifetime)\n", s.failed_total));

    match s.oldest_ready_age_secs {
        Some(age) => out.push_str(&format!(
            "\n  oldest pending (ready): {} old\n",
            humanize_age_secs(age)
        )),
        None => out.push_str("\n  oldest pending (ready): (none) ✓\n"),
    }
    out
}

/// Statuses `ff pm purge` is allowed to delete: terminal or never-started rows.
/// Live work (ready/claimed/in_progress/in_review/blocked/building/reviewing) is
/// NEVER purgeable — deleting a leased/running item would orphan its slot.
const PURGEABLE_STATUSES: &[&str] = &["idea", "done", "cancelled", "failed"];

fn is_purgeable_status(s: &str) -> bool {
    PURGEABLE_STATUSES.contains(&s)
}

/// Validate a purge request BEFORE touching the DB. Pure. Enforces (a) at least
/// one filter so a bare `ff pm purge` can't wipe the table, and (b) an explicit
/// `--status` must be a purgeable one (no deleting live work).
fn validate_purge_request(
    kind: Option<&str>,
    status: Option<&str>,
    project: Option<&str>,
    older_than: Option<&str>,
) -> Result<(), String> {
    if kind.is_none() && status.is_none() && project.is_none() && older_than.is_none() {
        return Err(
            "refusing to purge with no filter — pass at least one of --kind/--status/--project/--older-than"
                .to_string(),
        );
    }
    if let Some(s) = status
        && !is_purgeable_status(s)
    {
        return Err(format!(
            "status '{s}' is not purgeable (live work is protected); purgeable: {}",
            PURGEABLE_STATUSES.join("/")
        ));
    }
    Ok(())
}

/// The shared WHERE clause for the purge preview + delete. `$1` is the
/// purgeable-status floor (the safety net), `$2..$5` are the optional filters.
const PURGE_WHERE: &str = "status = ANY($1) \
     AND ($2::text IS NULL OR kind = $2) \
     AND ($3::text IS NULL OR status = $3) \
     AND ($4::text IS NULL OR project_id = $4) \
     AND ($5::text IS NULL OR created_at < NOW() - ($5 || ' seconds')::interval)";

/// `ff pm purge` — dry-run by default; `--yes` deletes.
async fn handle_pm_purge(
    pool: &sqlx::PgPool,
    kind: Option<&str>,
    status: Option<&str>,
    project: Option<&str>,
    older_than: Option<&str>,
    yes: bool,
    json: bool,
) -> Result<()> {
    validate_purge_request(kind, status, project, older_than).map_err(|e| anyhow::anyhow!(e))?;
    let age_secs: Option<String> = match older_than {
        Some(spec) => Some(
            crate::utils::parse_duration_secs(spec)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid --older-than '{spec}' (use e.g. 7d/48h/30m)")
                })?
                .to_string(),
        ),
        None => None,
    };
    let purgeable: Vec<String> = PURGEABLE_STATUSES.iter().map(|s| s.to_string()).collect();

    // Preview: per (status, kind) breakdown of what matches.
    let rows: Vec<(String, String, i64)> = sqlx::query_as(&format!(
        "SELECT status, COALESCE(kind, '(none)'), count(*) FROM work_items \
         WHERE {PURGE_WHERE} GROUP BY status, kind ORDER BY count(*) DESC, status, kind"
    ))
    .bind(&purgeable)
    .bind(kind)
    .bind(status)
    .bind(project)
    .bind(&age_secs)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("purge preview: {e}"))?;

    let total: i64 = rows.iter().map(|(_, _, n)| n).sum();

    if json && !yes {
        let breakdown: Vec<_> = rows
            .iter()
            .map(|(s, k, n)| serde_json::json!({"status": s, "kind": k, "count": n}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"dry_run": true, "would_delete": total, "breakdown": breakdown})
            )?
        );
        return Ok(());
    }

    if total == 0 {
        println!("(no matching purgeable work_items)");
        return Ok(());
    }

    if !yes {
        println!("{CYAN}▶ DRY-RUN — would delete {total} work_item(s):{RESET}");
        for (s, k, n) in &rows {
            println!("    {s:<12} {k:<16} {n}");
        }
        println!("\n{YELLOW}Re-run with --yes to delete.{RESET}");
        return Ok(());
    }

    let deleted = sqlx::query(&format!("DELETE FROM work_items WHERE {PURGE_WHERE}"))
        .bind(&purgeable)
        .bind(kind)
        .bind(status)
        .bind(project)
        .bind(&age_secs)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("purge delete: {e}"))?
        .rows_affected();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"deleted": deleted}))?
        );
    } else {
        println!("{GREEN}✓ deleted {deleted} work_item(s){RESET}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Row, postgres::PgPoolOptions};
    use std::str::FromStr;

    async fn bind_repo_temp_pool() -> Option<(sqlx::PgPool, sqlx::PgPool, String)> {
        let database_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let mut options = sqlx::postgres::PgConnectOptions::from_str(&database_url).ok()?;
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .ok()?;
        let db_name = format!("ff_bind_repo_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .ok()?;
        options = options.database(&db_name);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .ok()?;
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE projects (id TEXT PRIMARY KEY);
             CREATE TABLE project_repos (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                project_id TEXT NOT NULL REFERENCES projects(id),
                github_url TEXT NOT NULL,
                name TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             );
             CREATE TABLE work_items (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                project_id TEXT NOT NULL REFERENCES projects(id),
                status TEXT NOT NULL,
                attempts INT NOT NULL DEFAULT 0,
                last_error TEXT,
                repo_id UUID REFERENCES project_repos(id),
                repo_url TEXT,
                repo_path TEXT,
                priority TEXT NOT NULL DEFAULT 'normal',
                payload JSONB NOT NULL DEFAULT '{}',
                assigned_to TEXT,
                assigned_computer TEXT,
                base_sha TEXT,
                base_branch TEXT
             );
             CREATE TABLE computers (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL
             );
             CREATE TABLE sub_agents (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                current_work_item_id UUID REFERENCES work_items(id)
             );
             CREATE TABLE work_item_leases (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                work_item_id UUID NOT NULL REFERENCES work_items(id),
                computer_id UUID NOT NULL REFERENCES computers(id),
                lease_expires_at TIMESTAMPTZ NOT NULL,
                released_at TIMESTAMPTZ
             );
             CREATE TABLE work_item_events (
                id BIGSERIAL PRIMARY KEY,
                work_item_id UUID NOT NULL REFERENCES work_items(id),
                from_status TEXT,
                to_status TEXT NOT NULL,
                computer TEXT,
                attempt INT,
                detail TEXT
             );",
        )
        .execute(&pool)
        .await
        .expect("create bind-repo test schema");
        Some((admin, pool, db_name))
    }

    async fn drop_bind_repo_temp_pool(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
              WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .unwrap();
        sqlx::query(&format!("DROP DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }

    async fn seed_bind_repo(pool: &sqlx::PgPool) -> (uuid::Uuid, uuid::Uuid) {
        sqlx::query("INSERT INTO projects(id) VALUES ('p1'), ('p2')")
            .execute(pool)
            .await
            .unwrap();
        let repo1 = sqlx::query_scalar(
            "INSERT INTO project_repos(project_id, name, github_url) \
             VALUES ('p1', 'api', 'https://example.test/p1/api.git') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let repo2 = sqlx::query_scalar(
            "INSERT INTO project_repos(project_id, name, github_url) \
             VALUES ('p2', 'api', 'https://example.test/p2/api.git') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        (repo1, repo2)
    }

    async fn insert_bind_item(
        pool: &sqlx::PgPool,
        status: &str,
        error: Option<&str>,
    ) -> uuid::Uuid {
        sqlx::query_scalar(
            "INSERT INTO work_items \
               (project_id, status, attempts, last_error, repo_path, priority, payload, \
                assigned_to, assigned_computer, base_sha, base_branch) \
             VALUES ('p1', $1, 7, $2, '/explicit/path', 'high', '{\"keep\":true}', \
                     'alice', 'sia', 'abc123', 'develop') RETURNING id",
        )
        .bind(status)
        .bind(error)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test]
    fn repository_resolution_error_class_is_exact_prefix() {
        assert!(is_repository_resolution_failed_closed(
            "repository resolution failed closed: no registered project_repos row"
        ));
        assert!(!is_repository_resolution_failed_closed(
            "wrapper: repository resolution failed closed: no repo"
        ));
        assert!(!is_repository_resolution_failed_closed(
            "repository resolution failed closed: "
        ));
    }

    #[tokio::test]
    async fn bind_repo_ready_repair_preserves_fields_and_audits() {
        let Some((admin, pool, db_name)) = bind_repo_temp_pool().await else {
            return;
        };
        let (repo_id, _) = seed_bind_repo(&pool).await;
        let repair_error = "repository resolution failed closed: no registered project_repos row";
        let ready = insert_bind_item(&pool, "ready", Some(repair_error)).await;

        let ready_result = bind_work_item_repo(&pool, ready, "api", "tester")
            .await
            .unwrap();
        assert_eq!(ready_result.repo_id, repo_id);

        let row = sqlx::query(
            "SELECT status, attempts, last_error, repo_id, repo_url, repo_path, priority, \
                        payload, assigned_to, assigned_computer, base_sha, base_branch \
                   FROM work_items WHERE id = $1",
        )
        .bind(ready)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("status"), "ready");
        assert_eq!(row.get::<i32, _>("attempts"), 7);
        assert_eq!(row.get::<Option<String>, _>("last_error"), None);
        assert_eq!(row.get::<uuid::Uuid, _>("repo_id"), repo_id);
        assert_eq!(row.get::<String, _>("repo_path"), "/explicit/path");
        assert_eq!(row.get::<String, _>("priority"), "high");
        assert_eq!(row.get::<serde_json::Value, _>("payload")["keep"], true);
        assert_eq!(
            row.get::<Option<String>, _>("assigned_to").as_deref(),
            Some("alice")
        );
        assert_eq!(
            row.get::<Option<String>, _>("assigned_computer").as_deref(),
            Some("sia")
        );
        assert_eq!(
            row.get::<Option<String>, _>("base_sha").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            row.get::<Option<String>, _>("base_branch").as_deref(),
            Some("develop")
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM work_item_events \
                      WHERE work_item_id = $1 AND to_status = 'repo_bound'"
            )
            .bind(ready)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        drop_bind_repo_temp_pool(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn bind_repo_rejects_states_errors_selectors_and_live_ownership() {
        let Some((admin, pool, db_name)) = bind_repo_temp_pool().await else {
            return;
        };
        let (repo_id, cross_project_repo) = seed_bind_repo(&pool).await;
        sqlx::query(
            "INSERT INTO project_repos(project_id, name, github_url) \
             VALUES ('p1', 'duplicate', 'https://example.test/a'), \
                    ('p1', 'duplicate', 'https://example.test/b')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO project_repos(project_id, name, github_url) \
             VALUES ('p1', $1, 'https://example.test/uuid-shaped-name')",
        )
        .bind(repo_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let uuid_precedence = insert_bind_item(&pool, "ready", None).await;
        assert_eq!(
            bind_work_item_repo(&pool, uuid_precedence, &repo_id.to_string(), "tester")
                .await
                .unwrap()
                .repo_id,
            repo_id
        );
        for (status, error) in [
            ("idea", None),
            ("claimed", None),
            ("building", None),
            ("failed", Some("compile failed")),
            (
                "failed",
                Some("repository resolution failed closed: no registered project_repos row"),
            ),
        ] {
            let id = insert_bind_item(&pool, status, error).await;
            assert!(
                bind_work_item_repo(&pool, id, "api", "tester")
                    .await
                    .is_err()
            );
        }
        let selector_item = insert_bind_item(&pool, "ready", None).await;
        for selector in [
            cross_project_repo.to_string(),
            "duplicate".to_string(),
            "https://example.test/p1/api.git".to_string(),
        ] {
            assert!(
                bind_work_item_repo(&pool, selector_item, &selector, "tester")
                    .await
                    .is_err()
            );
        }

        let computer: uuid::Uuid =
            sqlx::query_scalar("INSERT INTO computers(name) VALUES ('sia') RETURNING id")
                .fetch_one(&pool)
                .await
                .unwrap();
        let active = insert_bind_item(&pool, "ready", None).await;
        sqlx::query(
            "INSERT INTO work_item_leases(work_item_id, computer_id, lease_expires_at) \
             VALUES ($1, $2, NOW() + interval '1 minute')",
        )
        .bind(active)
        .bind(computer)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            bind_work_item_repo(&pool, active, "api", "tester")
                .await
                .is_err()
        );
        let leased = insert_bind_item(&pool, "ready", None).await;
        sqlx::query(
            "INSERT INTO work_item_leases(work_item_id, computer_id, lease_expires_at) \
             VALUES ($1, $2, NOW() - interval '1 minute')",
        )
        .bind(leased)
        .bind(computer)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            bind_work_item_repo(&pool, leased, &repo_id.to_string(), "tester")
                .await
                .is_err()
        );
        sqlx::query("UPDATE work_item_leases SET released_at = NOW() WHERE work_item_id = $1")
            .bind(leased)
            .execute(&pool)
            .await
            .unwrap();
        bind_work_item_repo(&pool, leased, &repo_id.to_string(), "tester")
            .await
            .unwrap();

        let slotted = insert_bind_item(&pool, "ready", None).await;
        sqlx::query("INSERT INTO sub_agents(current_work_item_id) VALUES ($1)")
            .bind(slotted)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            bind_work_item_repo(&pool, slotted, "api", "tester")
                .await
                .is_err()
        );
        drop_bind_repo_temp_pool(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn bind_repo_is_idempotent_and_event_failure_rolls_back() {
        let Some((admin, pool, db_name)) = bind_repo_temp_pool().await else {
            return;
        };
        let (repo_id, _) = seed_bind_repo(&pool).await;
        let id = insert_bind_item(&pool, "ready", None).await;
        bind_work_item_repo(&pool, id, "api", "tester")
            .await
            .unwrap();
        let repeated = bind_work_item_repo(&pool, id, &repo_id.to_string(), "tester")
            .await
            .unwrap();
        assert!(repeated.already_bound);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM work_item_events WHERE work_item_id = $1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );

        let rollback_id = insert_bind_item(&pool, "ready", None).await;
        sqlx::raw_sql(
            "CREATE FUNCTION reject_repo_bound() RETURNS trigger LANGUAGE plpgsql AS $$ \
               BEGIN RAISE EXCEPTION 'audit unavailable'; END $$; \
             CREATE TRIGGER reject_repo_bound BEFORE INSERT ON work_item_events \
               FOR EACH ROW EXECUTE FUNCTION reject_repo_bound();",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            bind_work_item_repo(&pool, rollback_id, "api", "tester")
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<uuid::Uuid>>(
                "SELECT repo_id FROM work_items WHERE id = $1"
            )
            .bind(rollback_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            None
        );
        drop_bind_repo_temp_pool(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn bind_repo_serializes_with_claim_and_rechecks_status() {
        let Some((admin, pool, db_name)) = bind_repo_temp_pool().await else {
            return;
        };
        seed_bind_repo(&pool).await;
        let id = insert_bind_item(&pool, "ready", None).await;
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM work_items WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        let claim_pool = pool.clone();
        let claim = tokio::spawn(async move {
            sqlx::query(
                "UPDATE work_items SET status = 'claimed' \
                  WHERE id = $1 AND status = 'ready'",
            )
            .bind(id)
            .execute(&claim_pool)
            .await
            .unwrap()
            .rows_affected()
        });
        blocker.commit().await.unwrap();
        assert_eq!(claim.await.unwrap(), 1);
        assert!(
            bind_work_item_repo(&pool, id, "api", "tester")
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<uuid::Uuid>>(
                "SELECT repo_id FROM work_items WHERE id = $1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            None
        );
        drop_bind_repo_temp_pool(admin, pool, &db_name).await;
    }

    #[test]
    fn close_validation_rejects_blank_evidence_and_bad_verification_dependency() {
        assert!(validate_close_request(" \n", None, false, false).is_err());
        assert!(validate_close_request("proof", None, false, true).is_err());
        assert!(validate_close_request("proof", Some(" "), true, false).is_err());
        assert!(validate_close_request(" proof ", Some("abc"), true, true).is_ok());
    }

    #[test]
    fn close_evidence_preserves_prior_content_and_error_and_deduplicates() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-07-29T09:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (once, first_appended) = append_human_close_evidence(
            Some("existing acceptance"),
            Some("old failure"),
            "marcus",
            "tests pass",
            at,
        )
        .unwrap();
        let (twice, second_appended) = append_human_close_evidence(
            Some(&once),
            Some("old failure"),
            "adele",
            "tests pass",
            at + chrono::Duration::minutes(1),
        )
        .unwrap();
        assert!(first_appended);
        assert!(!second_appended);
        assert!(twice.starts_with("existing acceptance\n"));
        assert_eq!(twice.matches("prior-last-error: old failure").count(), 1);
        assert_eq!(twice.matches("human-close: ").count(), 1);
        assert!(twice.contains("\"actor\":\"marcus\""));
        assert!(twice.contains("\"evidence\":\"tests pass\""));
    }

    #[test]
    fn decomposition_quality_gate_merges_sibling_file_overlap() {
        let tasks = vec![
            LeafTask {
                title: "first".into(),
                description: "change parser".into(),
                files: vec!["src/lib.rs".into()],
                complexity: Some("mechanical".into()),
                acceptance_criteria: vec![],
            },
            LeafTask {
                title: "second".into(),
                description: "add parser test".into(),
                files: vec!["src/lib.rs".into(), "src/tests.rs".into()],
                complexity: Some("moderate".into()),
                acceptance_criteria: vec![],
            },
        ];
        // Tests the sibling-merge logic directly (the gate now fails-closed on a
        // None repo context, so exercise the merge function itself).
        let gated = merge_overlapping_siblings(tasks);
        assert_eq!(gated.len(), 1);
        assert_eq!(gated[0].files, ["src/lib.rs", "src/tests.rs"]);
        assert!(gated[0].description.contains("add parser test"));
        assert_eq!(gated[0].complexity.as_deref(), Some("moderate"));
    }

    #[test]
    fn backticked_symbols_ignores_paths_and_commands() {
        assert_eq!(
            backticked_symbols(
                "existing `LeaseManager` calls `lease::reap`; run `cargo test` and edit `src/lib.rs`"
            ),
            ["LeaseManager", "lease::reap"]
        );
    }

    #[test]
    fn decomposition_quality_gate_merges_transitive_overlap() {
        let task = |title: &str, files: &[&str]| LeafTask {
            title: title.into(),
            description: title.into(),
            files: files.iter().map(|file| (*file).into()).collect(),
            complexity: None,
            acceptance_criteria: vec![],
        };
        let gated = merge_overlapping_siblings(vec![
            task("first", &["a.rs"]),
            task("second", &["b.rs"]),
            task("bridge", &["a.rs", "b.rs"]),
        ]);
        assert_eq!(gated.len(), 1);
        assert!(gated[0].files.contains(&"a.rs".into()));
        assert!(gated[0].files.contains(&"b.rs".into()));
    }

    /// Drop guard around a throwaway git repo under the system temp dir.
    struct ScratchRepo {
        path: PathBuf,
    }

    impl Drop for ScratchRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Build a git repo with the given files staged (staged is enough for
    /// `git ls-files`). Returns None when git isn't available so callers can
    /// skip instead of panicking.
    fn scratch_git_repo(
        tag: &str,
        files: &[&str],
    ) -> Option<(ScratchRepo, crate::repo_context::RepoContext)> {
        let path = std::env::temp_dir().join(format!("ff-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).ok()?;
        let repo = ScratchRepo { path };
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo.path)
                .args(args)
                .output()
                .ok()
                .filter(|out| out.status.success())
        };
        git(&["init", "-q"])?;
        for file in files {
            let file_path = repo.path.join(file);
            std::fs::create_dir_all(file_path.parent()?).ok()?;
            std::fs::write(&file_path, "// scratch\n").ok()?;
        }
        git(&["add", "-A"])?;
        let ctx = crate::repo_context::RepoContext {
            repo_id: None,
            repo_url: None,
            repo_path: Some(repo.path.clone()),
            primary_language: "rust".to_string(),
            build_system: None,
            key_dirs: Vec::new(),
        };
        Some((repo, ctx))
    }

    fn leaf(title: &str, description: &str, files: &[&str]) -> LeafTask {
        LeafTask {
            title: title.into(),
            description: description.into(),
            files: files.iter().map(|file| (*file).into()).collect(),
            complexity: None,
            acceptance_criteria: vec![],
        }
    }

    #[test]
    fn decomposition_quality_gate_accepts_files_present_on_disk() {
        let Some((_repo, ctx)) = scratch_git_repo("present", &["src/lib.rs", "src/parser.rs"])
        else {
            return;
        };
        let tasks = vec![leaf(
            "edit",
            "change parser",
            &["src/lib.rs", "src/parser.rs"],
        )];
        let gated = quality_gate_decomposition(tasks, Some(&ctx)).unwrap();
        assert_eq!(gated.len(), 1);
    }

    #[test]
    fn decomposition_quality_gate_rejects_tracked_but_deleted_file() {
        // A deleted file stays in `git ls-files`, so tracking-based validation
        // alone passes it; the `find`-based disk check must reject it.
        let Some((repo, ctx)) = scratch_git_repo("deleted", &["src/lib.rs", "src/gone.rs"]) else {
            return;
        };
        std::fs::remove_file(repo.path.join("src/gone.rs")).unwrap();
        // A deleted file whose PARENT DIR still exists is now ALLOWED (recreation
        // in an existing module is legitimate feature work). Rejection now
        // requires the parent dir to be missing too — so this task passes.
        let tasks = vec![leaf("edit gone", "modify the handler", &["src/gone.rs"])];
        assert!(
            quality_gate_decomposition(tasks, Some(&ctx)).is_ok(),
            "recreating a file in an existing dir should be allowed"
        );
        // But a file whose parent dir does NOT exist is still rejected.
        let tasks = vec![leaf("bad", "x", &["nonexistent_dir/thing.rs"])];
        assert!(quality_gate_decomposition(tasks, Some(&ctx)).is_err());
    }

    #[test]
    fn decomposition_quality_gate_rejects_nonexistent_path() {
        let Some((_repo, ctx)) = scratch_git_repo("hallucinated", &["src/lib.rs"]) else {
            return;
        };
        let tasks = vec![leaf(
            "edit",
            "modify the handler",
            &["pkg/storage/factory.go"],
        )];
        assert!(quality_gate_decomposition(tasks, Some(&ctx)).is_err());
    }

    #[test]
    fn decomposition_quality_gate_rejects_create_worded_task_for_missing_file() {
        // NEW policy (2026-07-25): a new file whose PARENT DIR exists is ALLOWED
        // (real feature work = new module in an existing dir); a new file whose
        // parent dir does NOT exist is rejected (cross-language/wrong-structure
        // hallucination, e.g. `mcp/service.py` in a Rust repo).
        let Some((_repo, ctx)) = scratch_git_repo("create", &["src/lib.rs"]) else {
            return;
        };
        // src/ exists → new module allowed.
        let ok = vec![leaf(
            "Add metrics module",
            "Create a new file with counters",
            &["src/metrics.rs"],
        )];
        assert!(
            quality_gate_decomposition(ok, Some(&ctx)).is_ok(),
            "new file in an existing dir should be allowed"
        );
        // mcp/ does not exist → rejected as wrong-structure.
        let bad = vec![leaf("Add py service", "create it", &["mcp/service.py"])];
        assert!(quality_gate_decomposition(bad, Some(&ctx)).is_err());
    }

    #[test]
    fn find_file_exists_requires_exact_path_not_just_basename() {
        let Some((repo, _ctx)) = scratch_git_repo("find-exact", &["a/mod.rs"]) else {
            return;
        };
        assert!(find_file_exists(&repo.path, "a/mod.rs"));
        // Same basename elsewhere must not vouch for a nonexistent path.
        assert!(!find_file_exists(&repo.path, "b/mod.rs"));
    }

    #[test]
    fn claude_project_slug_encodes_every_non_alphanumeric() {
        // Dots become '-' too — the old '/'-only replace produced
        // `-home-adele-.forgefleet` which matches no real projects dir.
        assert_eq!(
            claude_project_slug("/home/adele/.forgefleet/sub-agents/sub-agent-0/forge-fleet"),
            "-home-adele--forgefleet-sub-agents-sub-agent-0-forge-fleet"
        );
        assert_eq!(claude_project_slug("/tmp/my_app v2"), "-tmp-my-app-v2");
    }

    #[test]
    fn claude_status_maps_to_work_item_vocabulary() {
        assert_eq!(claude_status_to_work_item("pending"), "backlog");
        assert_eq!(claude_status_to_work_item("in_progress"), "in_progress");
        assert_eq!(claude_status_to_work_item("completed"), "done");
        assert_eq!(claude_status_to_work_item("deleted"), "cancelled");
        assert_eq!(claude_status_to_work_item("cancelled"), "cancelled");
        assert_eq!(claude_status_to_work_item("unknown"), "backlog");
    }

    #[test]
    fn parse_legacy_task_text_lines() {
        let text = "#1 [pending] Update parser\n#2 [in_progress] Write tests\n#3 [completed] Ship it [blocked by #2]\n#4. [deleted] Old task";
        let parsed = parse_task_text_lines(text);
        assert_eq!(parsed.len(), 4);
        assert_eq!(
            parsed[0],
            ("1".into(), "pending".into(), "Update parser".into())
        );
        assert_eq!(
            parsed[1],
            ("2".into(), "in_progress".into(), "Write tests".into())
        );
        assert_eq!(
            parsed[2],
            ("3".into(), "completed".into(), "Ship it".into())
        );
        assert_eq!(parsed[3], ("4".into(), "deleted".into(), "Old task".into()));
    }

    #[test]
    fn parse_claude_tasks_legacy_text_format() {
        let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Here are the tasks:\n#1 [pending] Update parser\n#2 [completed] Write tests"}]}}"#;
        let tasks = parse_claude_tasks(transcript);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks["1"].subject, "Update parser");
        assert_eq!(tasks["1"].status, "pending");
        assert_eq!(tasks["2"].subject, "Write tests");
        assert_eq!(tasks["2"].status, "completed");
    }

    #[test]
    fn parse_claude_tasks_todo_write() {
        let transcript = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_todo",
                    "name": "TodoWrite",
                    "input": {
                        "todos": [
                            {"content": "Design schema", "status": "pending", "activeForm": "Designing schema"},
                            {"content": "Write tests", "status": "in_progress", "activeForm": "Writing tests"}
                        ]
                    }
                }]
            }
        });
        let tasks = parse_claude_tasks(&serde_json::to_string(&transcript).unwrap());
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks["1"].subject, "Design schema");
        assert_eq!(tasks["1"].status, "pending");
        assert_eq!(tasks["2"].subject, "Write tests");
        assert_eq!(tasks["2"].status, "in_progress");
    }

    #[test]
    fn parse_claude_tasks_task_create_update() {
        let lines = [
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_create",
                        "name": "TaskCreate",
                        "input": {"subject": "Refactor parser", "description": "Split into helpers"}
                    }]
                }
            }),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_create",
                        "content": {"id": "7", "subject": "Refactor parser", "status": "pending"}
                    }]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_update",
                        "name": "TaskUpdate",
                        "input": {"taskId": "7", "status": "completed"}
                    }]
                }
            }),
        ];
        let transcript = lines
            .iter()
            .map(|l| serde_json::to_string(l).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let tasks = parse_claude_tasks(&transcript);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks["7"].subject, "Refactor parser");
        assert_eq!(tasks["7"].description, "Split into helpers");
        assert_eq!(tasks["7"].status, "completed");
    }

    #[test]
    fn parse_claude_tasks_task_list_json_result() {
        let transcript = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_list",
                    "content": [
                        {"type": "text", "text": "[{\"id\": \"1\", \"subject\": \"A\", \"status\": \"pending\"}, {\"id\": \"2\", \"subject\": \"B\", \"status\": \"completed\"}]"}
                    ]
                }]
            }
        });
        let tasks = parse_claude_tasks(&serde_json::to_string(&transcript).unwrap());
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks["1"].subject, "A");
        assert_eq!(tasks["1"].status, "pending");
        assert_eq!(tasks["2"].subject, "B");
        assert_eq!(tasks["2"].status, "completed");
    }

    #[test]
    fn parse_claude_tasks_task_get_object_result() {
        let transcript = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_get",
                    "content": {"id": "42", "subject": "Deep task", "description": "Details here", "status": "in_progress"}
                }]
            }
        });
        let tasks = parse_claude_tasks(&serde_json::to_string(&transcript).unwrap());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks["42"].subject, "Deep task");
        assert_eq!(tasks["42"].description, "Details here");
        assert_eq!(tasks["42"].status, "in_progress");
    }

    #[test]
    fn parse_claude_tasks_task_create_returns_tasks_array() {
        let lines = [
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_create",
                        "name": "TaskCreate",
                        "input": {"subject": "Build auth", "description": "JWT auth"}
                    }]
                }
            }),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_create",
                        "content": {
                            "success": true,
                            "tasks": [
                                {"id": "1", "subject": "Build auth", "description": "JWT auth", "status": "pending"},
                                {"id": "2", "subject": "Write tests", "status": "pending"}
                            ]
                        }
                    }]
                }
            }),
        ];
        let transcript = lines
            .iter()
            .map(|l| serde_json::to_string(l).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let tasks = parse_claude_tasks(&transcript);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks["1"].subject, "Build auth");
        assert_eq!(tasks["1"].description, "JWT auth");
        assert_eq!(tasks["2"].subject, "Write tests");
    }

    #[test]
    fn parse_claude_tasks_title_field() {
        let transcript = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_list",
                    "content": [
                        {"type": "text", "text": "[{\"id\": \"t-1\", \"title\": \"Use title field\", \"status\": \"in_progress\"}]"}
                    ]
                }]
            }
        });
        let tasks = parse_claude_tasks(&serde_json::to_string(&transcript).unwrap());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks["t-1"].subject, "Use title field");
        assert_eq!(tasks["t-1"].status, "in_progress");
    }

    #[test]
    fn parse_claude_tasks_task_update_with_id() {
        let lines = [
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_create",
                        "name": "TaskCreate",
                        "input": {"subject": "Update me"}
                    }]
                }
            }),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_create",
                        "content": "#1 created: Update me"
                    }]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_update",
                        "name": "TaskUpdate",
                        "input": {"id": "1", "status": "completed"}
                    }]
                }
            }),
        ];
        let transcript = lines
            .iter()
            .map(|l| serde_json::to_string(l).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let tasks = parse_claude_tasks(&transcript);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks["1"].subject, "Update me");
        assert_eq!(tasks["1"].status, "completed");
    }

    #[test]
    fn decomposed_work_item_insert_persists_repo_binding() {
        let sql = decomposed_work_item_insert_sql();
        assert!(sql.contains("repo_id"));
        assert!(sql.contains("repo_url"));
        assert!(sql.contains("repo_path"));
        assert!(sql.contains("$6, $7, $8"));
    }

    #[test]
    fn decomposed_work_item_insert_persists_scope_and_complexity() {
        let sql = decomposed_work_item_insert_sql();
        assert!(sql.contains("predicted_paths"));
        assert!(sql.contains("complexity"));
        // 12 bound columns now (was 10) → placeholders through $10.
        assert!(sql.contains("$9, $10"));
    }

    #[test]
    fn source_dir_summary_dedups_dirs_filters_noise() {
        let ls = "crates/ff-terminal/src/pm_cmd.rs\n\
                  crates/ff-terminal/src/main.rs\n\
                  crates/ff-agent/src/lib.rs\n\
                  crates/ff-agent/target/debug/junk.rs\n\
                  node_modules/pkg/index.js\n\
                  README.md\n\
                  Cargo.toml";
        let out = source_dir_summary(ls, 120).expect("has source dirs");
        // Deduped dir with count.
        assert!(out.contains("- crates/ff-terminal/src/ (2 files)"));
        assert!(out.contains("- crates/ff-agent/src/ (1 files)"));
        // target/ and node_modules/ filtered out; .md not a source ext.
        assert!(!out.contains("target/debug"));
        assert!(!out.contains("node_modules"));
        assert!(!out.contains("README.md"));
        // Root-level Cargo.toml → "." dir.
        assert!(out.contains("- ./ (1 files)"));
    }

    #[test]
    fn source_dir_summary_none_when_no_source() {
        assert!(source_dir_summary("README.md\nLICENSE\n", 120).is_none());
    }

    #[test]
    fn workspace_crate_hint_preserves_ff_agent_prefix() {
        let ls = "crates/ff-agent/src/lib.rs\ncrates/ff-terminal/src/main.rs\n";
        let hint = workspace_crate_hint(ls, "Fix task generation for ff-agent crate")
            .expect("ff-agent is a tracked workspace crate named in the goal");
        assert!(hint.contains("`ff-agent` => `crates/ff-agent/`"));
        assert!(hint.contains("never shorten them to `src/...`"));
    }

    #[test]
    fn workspace_crate_hint_matches_underscored_ff_core_reference() {
        let ls = "crates/ff-core/src/task.rs\ncrates/ff-terminal/src/main.rs\n";
        // Rust code and prose commonly reference this crate as `ff_core`
        // (its import name), not `ff-core` (its Cargo directory name).
        let hint = workspace_crate_hint(ls, "Adjust ff_core task generation").expect(
            "ff-core is a tracked workspace crate named in the goal via its underscored form",
        );
        assert!(hint.contains("`ff-core` => `crates/ff-core/`"));
        assert!(hint.contains("never shorten them to `src/...`"));
    }

    #[test]
    fn workspace_crate_hint_still_matches_hyphenated_ff_core_reference() {
        let ls = "crates/ff-core/src/task.rs\n";
        let hint = workspace_crate_hint(ls, "Adjust ff-core task generation")
            .expect("ff-core is a tracked workspace crate named in the goal");
        assert!(hint.contains("`ff-core` => `crates/ff-core/`"));
    }

    #[test]
    fn normalize_complexity_maps_to_vocabulary() {
        assert_eq!(normalize_complexity(Some("COMPLEX")), "complex");
        assert_eq!(normalize_complexity(Some(" moderate ")), "moderate");
        assert_eq!(normalize_complexity(Some("mechanical")), "mechanical");
        // Unknown / absent / junk → the safe default.
        assert_eq!(normalize_complexity(Some("epic")), "mechanical");
        assert_eq!(normalize_complexity(None), "mechanical");
    }

    fn sample() -> PmStats {
        PmStats {
            total: 870,
            by_status: vec![
                LabeledCount {
                    label: "idea".into(),
                    count: 784,
                },
                LabeledCount {
                    label: "done".into(),
                    count: 50,
                },
            ],
            by_kind: vec![LabeledCount {
                label: "audit".into(),
                count: 777,
            }],
            by_project: vec![LabeledCount {
                label: "forge-fleet".into(),
                count: 870,
            }],
            created_24h: 3,
            created_7d: 40,
            completed_24h: 2,
            completed_7d: 12,
            failed_total: 4,
            oldest_ready_age_secs: Some(9000),
        }
    }

    #[test]
    fn humanize_age_buckets() {
        assert_eq!(humanize_age_secs(30), "30s");
        assert_eq!(humanize_age_secs(90), "1m");
        assert_eq!(humanize_age_secs(7200), "2h");
        assert_eq!(humanize_age_secs(180_000), "2d");
    }

    #[test]
    fn render_includes_totals_sections_and_throughput() {
        let out = render_pm_stats(&sample());
        assert!(out.contains("work_items — 870 rows total"));
        assert!(out.contains("by status:"));
        assert!(out.contains("idea           784"));
        assert!(out.contains("by kind:"));
        assert!(out.contains("audit          777"));
        assert!(out.contains("created    3 in 24h · 40 in 7d"));
        assert!(out.contains("completed  2 in 24h · 12 in 7d"));
        assert!(out.contains("failed     4 (lifetime)"));
        // 9000s = 2h30m → coarsened to "2h"
        assert!(out.contains("oldest pending (ready): 2h old"));
    }

    #[test]
    fn render_handles_empty_and_no_ready() {
        let mut s = sample();
        s.by_status.clear();
        s.oldest_ready_age_secs = None;
        let out = render_pm_stats(&s);
        assert!(out.contains("by status:\n    (none)"));
        assert!(out.contains("oldest pending (ready): (none) ✓"));
    }

    #[test]
    fn project_fairness_uses_scheduler_capacity_formula() {
        assert_eq!(project_fair_share(3, 10), 4);
        assert_eq!(project_fair_share(3, 9), 3);
        assert_eq!(project_fair_share(0, 7), 7);
    }

    #[test]
    fn project_fairness_renders_active_work_and_allocation() {
        let projects = vec![
            (Some("forge-fleet".to_string()), 5),
            (Some("hireflow360".to_string()), 1),
            (None, 2),
        ];
        let out = render_project_fairness(&projects, 4);

        assert!(out.contains("PROJECT"));
        assert!(out.contains("ACTIVE WORK"));
        assert!(out.contains("FAIR SHARE"));
        assert!(out.contains("forge-fleet"));
        assert!(out.contains("hireflow360"));
        assert!(out.contains("<unassigned>"));
        assert_eq!(out.matches('4').count(), 3);
    }

    #[test]
    fn jira_dispatch_doctor_uses_canonical_binding_and_existing_hold_timestamp() {
        let sql = jira_dispatch_doctor_sql();
        assert!(sql.contains("metadata->>'jira_status'"));
        assert!(sql.contains("metadata->>'jira_held_at'"));
        assert!(sql.contains("project_repos"));
        assert!(sql.contains("pr.id = w.repo_id AND pr.project_id = w.project_id"));
        assert!(sql.contains("WITH RECURSIVE jira_lineage"));
    }

    #[test]
    fn jira_dispatch_doctor_fail_output_is_concise_and_complete() {
        let out = render_jira_dispatch_diagnostics(1, 2, 3, 4);
        assert!(out.contains("blocked parents ready 1"));
        assert!(out.contains("live children 2"));
        assert!(out.contains("no canonical repo 3"));
        assert!(out.contains("stale holds 4"));
        assert_eq!(out.lines().count(), 1);

        let healthy = render_jira_dispatch_diagnostics(0, 0, 0, 0);
        assert!(healthy.contains("Jira dispatch invariants"));
        assert!(healthy.contains("clear"));
    }

    #[test]
    fn purge_only_terminal_statuses_are_purgeable() {
        for s in ["idea", "done", "cancelled", "failed"] {
            assert!(is_purgeable_status(s), "{s} should be purgeable");
        }
        // Live statuses must NEVER be purgeable.
        for s in ["ready", "claimed", "in_progress", "in_review", "blocked"] {
            assert!(!is_purgeable_status(s), "{s} must be protected");
        }
    }

    #[test]
    fn purge_validation_requires_a_filter() {
        // A bare purge (no filter) is refused so it can't wipe the table.
        assert!(validate_purge_request(None, None, None, None).is_err());
        // Any single filter is enough.
        assert!(validate_purge_request(Some("audit"), None, None, None).is_ok());
        assert!(validate_purge_request(None, None, None, Some("30d")).is_ok());
    }

    #[test]
    fn purge_validation_rejects_live_status() {
        // Asking to purge a live status is rejected even though a filter is set.
        assert!(validate_purge_request(None, Some("in_progress"), None, None).is_err());
        assert!(validate_purge_request(None, Some("ready"), None, None).is_err());
        // A purgeable status passes.
        assert!(validate_purge_request(None, Some("idea"), None, None).is_ok());
    }

    #[test]
    fn decompose_insert_persists_repo_binding_columns() {
        let sql = decomposed_work_item_insert_sql();
        assert!(sql.contains("repo_id"));
        assert!(sql.contains("repo_url"));
        assert!(sql.contains("repo_path"));
        assert!(sql.contains("$6, $7, $8"));
    }
}
