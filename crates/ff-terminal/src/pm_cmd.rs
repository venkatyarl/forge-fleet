use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;
use std::path::PathBuf;

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

            let created_by = ff_agent::fleet_info::resolve_this_worker_name().await;
            let prio = priority.unwrap_or_else(|| "normal".to_string());
            let row: (uuid::Uuid,) = sqlx::query_as(
                "INSERT INTO work_items (project_id, kind, title, description, priority, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 RETURNING id",
            )
            .bind(&project)
            .bind(&kind)
            .bind(&title)
            .bind(description.as_deref())
            .bind(&prio)
            .bind(&created_by)
            .fetch_one(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert work item: {e}"))?;

            println!("{GREEN}✓ Created work item{RESET}");
            println!("  id:       {}", row.0);
            println!("  project:  {project}");
            println!("  kind:     {kind}");
            println!("  title:    {title}");
            println!("  priority: {prio}");
            println!("  created_by: {created_by}");
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
            )> = sqlx::query_as(
                "SELECT w.kind, w.title, w.status, w.assigned_computer, lc.name AS live_host, \
                        wt.status AS worktree, mq.status AS merge_q, w.pr_url, w.repo_url, w.repo_path \
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
                println!(
                    "{:<8} {:<34} {:<11} {:<8} {:<11} {:<18} {}",
                    kind,
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
            // Release any active lease + free the slot, then mark terminal.
            sqlx::query(
                "UPDATE sub_agents SET current_work_item_id = NULL, status = 'idle' \
                  WHERE current_work_item_id = $1",
            )
            .bind(uid)
            .execute(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("free slot: {e}"))?;
            sqlx::query(
                "UPDATE work_item_leases \
                    SET released_at = NOW(), lease_state = 'released', release_reason = 'cancelled' \
                  WHERE work_item_id = $1 AND released_at IS NULL",
            )
            .bind(uid)
            .execute(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("release lease: {e}"))?;
            // Drop it out of the merge queue so the drain stops considering it
            // (the per-host worktree reaper cleans the on-disk worktree).
            let _ = sqlx::query(
                "UPDATE work_item_merge_queue \
                    SET status = 'failed', failed_at = NOW(), failure_reason = 'work item cancelled' \
                  WHERE work_item_id = $1 AND status NOT IN ('merged')",
            )
            .bind(uid)
            .execute(&pool)
            .await;
            let n = sqlx::query("UPDATE work_items SET status = 'cancelled' WHERE id = $1")
                .bind(uid)
                .execute(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("cancel work item: {e}"))?
                .rows_affected();
            if n == 0 {
                return Err(anyhow::anyhow!("no work item with id {id}"));
            }
            println!("{GREEN}✓ work item {id} cancelled{RESET} (lease released, slot freed)");
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

    // Orphaned `in_progress` work_items with NO active lease — invisible to the
    // lease-based reaper, so they sit forever. The scheduler's orphan sweep
    // cancels them after an hour; surface any here so "healthy" isn't a lie.
    let orphaned_in_progress = ff_db::pg_count_orphaned_work_items(pool, 3600)
        .await
        .map_err(|e| anyhow::anyhow!("doctor orphaned work_items: {e}"))?;

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

    println!();
    if leader_ok && stale_ok && slots_ok && orphans_ok {
        println!("{GREEN}✓ Summary:{RESET} Pillar-4 work_item pipeline is healthy");
    } else {
        println!("{RED}⚠ Summary:{RESET} Pillar-4 work_item pipeline needs attention");
    }

    Ok(())
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
    let (parent_id, goal_text, parent_repo): (
        Option<uuid::Uuid>,
        String,
        Option<crate::repo_context::RepoContext>,
    ) = match uuid::Uuid::parse_str(goal.trim()) {
        Ok(uid) => {
            let row: Option<(
                    String,
                    Option<String>,
                    Option<uuid::Uuid>,
                    Option<String>,
                    Option<String>,
                )> =
                    sqlx::query_as(
                        "SELECT title, description, repo_id, repo_url, repo_path FROM work_items WHERE id = $1",
                    )
                        .bind(uid)
                        .fetch_optional(pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("load work item {uid}: {e}"))?;
            match row {
                Some((title, desc, repo_id, repo_url, repo_path)) => (
                    Some(uid),
                    format!("{title}\n\n{}", desc.unwrap_or_default()),
                    repo_context_from_binding(repo_id, repo_url, repo_path),
                ),
                None => return Err(anyhow::anyhow!("no work item with id {goal}")),
            }
        }
        Err(_) => (None, goal.clone(), None),
    };

    let repo_context = if repo.is_none() && cwd.is_none() && parent_repo.is_some() {
        parent_repo
    } else {
        crate::repo_context::resolve_repo_context(pool, &project, cwd, repo.as_deref()).await?
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
    let prompt = format!(
        "You are a senior engineer breaking a goal into independent, well-scoped \
         LEAF coding tasks for the target repository. Each task must be doable \
         on its own by a coding agent and touch a small, specific set of files. \
         Plan against the target repository context below. Do not use the \
         project's primary repository unless it is the target repository.\n\n\
         {repo_block}\
         Output ONLY a JSON array (no prose, no markdown fence) of at most {max} \
         objects, each: {{\"title\": \"<imperative, <70 chars>\", \"description\": \
         \"<precise instructions: which file(s), what to add/change, mirror an \
         existing pattern; name the correct test/build command for this repo \
         when useful>\"}}. \
         GOAL:\n{goal_text}"
    );

    let resp = ff_agent::fleet_oneshot::fleet_oneshot(
        pool,
        &prompt,
        llm.as_deref(),
        Some(std::time::Duration::from_secs(180)),
    )
    .await
    .map_err(|e| anyhow::anyhow!("fleet_oneshot decompose: {e}"))?;

    // Pull the JSON array out of the completion (models sometimes wrap it).
    let text = resp.text.trim();
    let json_slice = match (text.find('['), text.rfind(']')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => {
            return Err(anyhow::anyhow!(
                "LLM did not return a JSON array; got: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
    };
    #[derive(serde::Deserialize)]
    struct LeafTask {
        title: String,
        description: String,
    }
    let tasks: Vec<LeafTask> = serde_json::from_str(json_slice)
        .map_err(|e| anyhow::anyhow!("parse decomposed tasks JSON: {e}"))?;
    if tasks.is_empty() {
        return Err(anyhow::anyhow!("LLM returned zero tasks"));
    }

    let created_by = ff_agent::fleet_info::resolve_this_worker_name().await;
    println!(
        "{GREEN}✓ {} leaf task(s) from {}{RESET}",
        tasks.len(),
        resp.worker_name
    );
    let mut ids = Vec::new();
    for t in tasks.into_iter().take(max) {
        let row = insert_decomposed_work_item(
            pool,
            &project,
            &t.title,
            &t.description,
            &created_by,
            parent_id,
            repo_context.as_ref(),
        )
        .await?;
        ids.push(row.0);
        println!("  + {} {}", row.0, t.title);
    }

    if ready {
        for id in &ids {
            sqlx::query("UPDATE work_items SET status = 'ready' WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await
                .map_err(|e| anyhow::anyhow!("flag ready {id}: {e}"))?;
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

async fn insert_decomposed_work_item(
    pool: &sqlx::PgPool,
    project: &str,
    title: &str,
    description: &str,
    created_by: &str,
    parent_id: Option<uuid::Uuid>,
    repo_context: Option<&crate::repo_context::RepoContext>,
) -> Result<(uuid::Uuid,)> {
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
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("insert child task: {e}"))
}

fn decomposed_work_item_insert_sql() -> &'static str {
    "INSERT INTO work_items \
        (project_id, kind, title, description, priority, created_by, parent_id, repo_id, repo_url, repo_path) \
     VALUES ($1, 'task', $2, $3, 'normal', $4, $5, $6, $7, $8) RETURNING id"
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
        let slug = cwd.to_string_lossy().replace('/', "-");
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
            .ok_or_else(|| anyhow::anyhow!("no session JSONL found under {:?}", project_dir))?
    };

    println!(
        "{CYAN}▶ Importing Claude tasks from{RESET} {}",
        resolved.display()
    );

    // Stream the JSONL, tracking the LAST task-list snapshot. Each line
    // we care about has content like `#<N> [<status>] <subject>` — we
    // find them inside tool_result `content` strings.
    let content = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {e}", resolved.display()))?;
    let task_line_re =
        regex::Regex::new(r"#(\d+)\s*(?:\.\s*)?\[(pending|in_progress|completed|deleted)\]\s+(.+)")
            .map_err(|e| anyhow::anyhow!("regex: {e}"))?;

    // Group by task_id — later occurrences overwrite earlier ones.
    let mut snapshot: std::collections::BTreeMap<String, (String, String)> = Default::default();
    for line in content.lines() {
        // Only look inside lines that mention system-reminder OR TaskList-shaped content.
        if !line.contains("[pending]")
            && !line.contains("[completed]")
            && !line.contains("[in_progress]")
        {
            continue;
        }
        for cap in task_line_re.captures_iter(line) {
            let id = cap[1].to_string();
            let status = cap[2].to_string();
            let mut subject = cap[3].trim().to_string();
            // Subject ends at the end of the match; may have trailing JSON
            // escape chars. Trim at the first of a few known terminators.
            for term in ["\\n", "\"", "\n"] {
                if let Some(pos) = subject.find(term) {
                    subject.truncate(pos);
                }
            }
            let subject = subject.trim_end().to_string();
            if !subject.is_empty() {
                snapshot.insert(id, (status, subject));
            }
        }
    }

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
        for (id, (status, subject)) in &snapshot {
            let clip = if subject.chars().count() > 60 {
                format!("{}…", subject.chars().take(59).collect::<String>())
            } else {
                subject.clone()
            };
            println!("  would upsert #{id:<3} [{status:<11}] {clip}");
        }
        return Ok(());
    }

    let mut inserted = 0usize;
    let mut updated = 0usize;
    for (id, (status, subject)) in &snapshot {
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

        let wi_status = match status.as_str() {
            "pending" => "backlog",
            "in_progress" => "in_progress",
            "completed" => "done",
            _ => "backlog",
        };

        if let Some(wi_id) = existing {
            sqlx::query(
                "UPDATE work_items
                    SET status = $1,
                        title  = $2
                  WHERE id = $3",
            )
            .bind(wi_status)
            .bind(subject)
            .bind(wi_id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("update work_item: {e}"))?;
            updated += 1;
        } else {
            sqlx::query(
                "INSERT INTO work_items
                    (project_id, kind, title, status, priority, created_by, metadata)
                 VALUES ($1, 'code', $2, $3, 'normal', 'claude_code',
                         jsonb_build_object('claude_task_id', $4::text,
                                            'imported_at', NOW()::text))",
            )
            .bind(project)
            .bind(subject)
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

    #[test]
    fn decomposed_work_item_insert_persists_repo_binding() {
        let sql = decomposed_work_item_insert_sql();
        assert!(sql.contains("repo_id"));
        assert!(sql.contains("repo_url"));
        assert!(sql.contains("repo_path"));
        assert!(sql.contains("$6, $7, $8"));
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
