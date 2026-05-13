//! `ff agent` subcommand implementations.

use anyhow::Result;

use crate::{CYAN, GREEN, RESET, YELLOW, resolve_pulse_redis_url};

pub async fn handle_agent_fanout(
    pool: &sqlx::PgPool,
    prompt: String,
    backend: String,
    fanout: u32,
) -> Result<()> {
    use ff_agent::cli_executor::backend_by_name;
    let cfg = backend_by_name(&backend).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown backend '{backend}'; expected one of: claude, codex, gemini, kimi, grok"
        )
    })?;

    // Parent compound task — gives the user a single UUID to watch.
    let leader_computer_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT computer_id FROM fleet_leader_state LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', $1, $2, 80, $3)
        RETURNING id
        "#,
    )
    .bind(format!(
        "agent-fanout: {} copies via backend={}",
        fanout, cfg.name
    ))
    .bind(serde_json::json!({
        "kind": "agent_fanout",
        "backend": cfg.name,
        "fanout": fanout,
        "prompt_preview": prompt.chars().take(200).collect::<String>(),
    }))
    .bind(leader_computer_id)
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("insert parent: {e}"))?;

    // Encode the prompt as a single-quoted shell argument. Replace any
    // single-quote with `'\''` so embedded quotes survive.
    let shell_safe_prompt = prompt.replace('\'', "'\\''");
    let cmd = format!("ff run --backend {} '{shell_safe_prompt}'", cfg.name);
    for i in 0..fanout {
        ff_agent::task_runner::pg_enqueue_shell_task(
            pool,
            &format!("agent-fanout/{i}: {} backend={}", cfg.name, cfg.name),
            &cmd,
            &[cfg.name.to_string()],
            None,
            Some(parent),
            70,
            leader_computer_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("enqueue child {i}: {e}"))?;
    }

    println!("composed parent task: {parent}");
    println!("watch progress with: ff tasks list --status pending,running --show-id");
    Ok(())
}

/// One shell task per capable member: the same prompt runs on every
/// member that advertises capability `[backend]`. Useful for "have
/// every member summarise their own logs in parallel" patterns.
pub async fn handle_agent_dispatch_each(
    pool: &sqlx::PgPool,
    prompt: String,
    backend: String,
) -> Result<()> {
    use ff_agent::cli_executor::backend_by_name;
    let cfg = backend_by_name(&backend).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown backend '{backend}'; expected one of: claude, codex, gemini, kimi, grok"
        )
    })?;

    // Find every member whose advertised capability set includes the
    // backend tag. Capabilities are computed on daemon startup (see
    // src/main.rs ~line 2152) and stored implicitly in fleet_workers
    // via the worker registration. Here we approximate by querying
    // computers whose status='ok' — the per-task `requires_capability`
    // matcher will skip incapable members at claim time anyway, so a
    // task to a member without the backend simply stays pending.
    let members: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM computers WHERE status IN ('ok', 'pending')")
            .fetch_all(pool)
            .await
            .map_err(|e| anyhow::anyhow!("list computers: {e}"))?;

    let leader_computer_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT computer_id FROM fleet_leader_state LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (task_type, summary, payload, priority, created_by_computer_id)
        VALUES ('compound', $1, $2, 80, $3)
        RETURNING id
        "#,
    )
    .bind(format!(
        "agent-dispatch-each: {} member(s) via backend={}",
        members.len(),
        cfg.name
    ))
    .bind(serde_json::json!({
        "kind": "agent_dispatch_each",
        "backend": cfg.name,
        "members": members.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(),
        "prompt_preview": prompt.chars().take(200).collect::<String>(),
    }))
    .bind(leader_computer_id)
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("insert parent: {e}"))?;

    let shell_safe_prompt = prompt.replace('\'', "'\\''");
    let cmd = format!("ff run --backend {} '{shell_safe_prompt}'", cfg.name);
    for (_id, name) in &members {
        ff_agent::task_runner::pg_enqueue_shell_task(
            pool,
            &format!("agent-dispatch-each: {} on {}", cfg.name, name),
            &cmd,
            &[cfg.name.to_string()],
            Some(name),
            Some(parent),
            70,
            leader_computer_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("enqueue task on {name}: {e}"))?;
    }

    println!("composed parent task: {parent}");
    println!("watch progress with: ff tasks list --status pending,running --show-id");
    Ok(())
}

// ─── #118: ff agent commit-back — fleet-LLM work → PR on origin/main ────────
//
// Lifts code produced by a fleet LLM in a sub-agent workspace back to Taylor's
// canonical repo via a feature branch + (optional) PR against origin/main.
//
// Flow:
//   1. Look up `work_outputs` WHERE agent_session_id = <session>. Pick the
//      latest row. Extract `produced_on_computer`, `modified_files`, title.
//   2. Resolve the worker's ssh_user + primary_ip from `fleet_workers`.
//      Resolve the canonical source-tree path via `software_registry.install_path`
//      (falls back to `~/.forgefleet/sub-agent-0/forge-fleet` per convention).
//   3. SSH into the worker and run git checkout -b / add / commit / (push / gh pr create).
//   4. Persist the resulting branch + PR URL back into `work_items.pr_url`
//      (via the work_item linked to the work_output).
//   5. Best-effort publish `fleet.events.agent.commit_back_completed` on NATS.
pub async fn handle_agent_commit_back(
    pool: &sqlx::PgPool,
    session_id: &str,
    push: bool,
    pr: bool,
) -> Result<()> {
    use tokio::process::Command;

    // 1. Look up the latest work_output for this session.
    let row: Option<(
        uuid::Uuid,        // work_output.id
        uuid::Uuid,        // work_item_id
        Option<String>,    // title
        Option<String>,    // produced_on_computer
        serde_json::Value, // modified_files
        Option<String>,    // llm_model_id
        Option<i32>,       // llm_tokens_input
        Option<i32>,       // llm_tokens_output
    )> = sqlx::query_as(
        "SELECT id, work_item_id, title, produced_on_computer, modified_files, \
                llm_model_id, llm_tokens_input, llm_tokens_output \
         FROM work_outputs \
         WHERE agent_session_id = $1 \
         ORDER BY produced_at DESC \
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query work_outputs: {e}"))?;

    let (wo_id, work_item_id, title, worker, modified_files_json, model_id, tok_in, tok_out) = row
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no work_outputs row with agent_session_id={session_id} — \
             was the session persisted, and did it produce a work_output?"
            )
        })?;

    let worker = worker.ok_or_else(|| {
        anyhow::anyhow!("work_output {wo_id} has no produced_on_computer — cannot locate worker")
    })?;

    let modified_files: Vec<String> = serde_json::from_value(modified_files_json.clone())
        .map_err(|e| anyhow::anyhow!("modified_files is not a JSON string array: {e}"))?;
    if modified_files.is_empty() {
        return Err(anyhow::anyhow!(
            "work_output {wo_id} has no modified_files — nothing to commit"
        ));
    }

    // 2. Resolve SSH target + workspace path.
    let (ssh_user, primary_ip): (String, String) =
        sqlx::query_as("SELECT ssh_user, ip FROM fleet_workers WHERE name = $1")
            .bind(&worker)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("lookup fleet_workers: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("no fleet_workers row for computer={worker}"))?;

    // Per reference_source_tree_locations.md: non-Taylor members use
    // ~/.forgefleet/sub-agent-0/forge-fleet. Taylor itself uses ~/projects/forge-fleet.
    let workspace = if worker.eq_ignore_ascii_case("taylor") {
        "~/projects/forge-fleet"
    } else {
        "~/.forgefleet/sub-agent-0/forge-fleet"
    };

    // 3. Build branch name: fleet/<worker>/<yyyymmdd>-<slug>.
    let now = chrono::Utc::now();
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let title_slug = slugify_for_branch(title.as_deref().unwrap_or("agent-session"));
    let branch_name = format!("fleet/{}/{stamp}-{title_slug}", worker);

    let commit_msg = format!(
        "{}\n\nProduced by ff agent on {worker} in session {session_id}.\n\n\
         Co-Authored-By: ForgeFleet Agent <agent@forgefleet.local>",
        title.as_deref().unwrap_or("ff agent commit-back")
    );

    eprintln!("{CYAN}▶ ff agent commit-back{RESET}");
    eprintln!("  session:   {session_id}");
    eprintln!("  worker:    {worker} ({ssh_user}@{primary_ip})");
    eprintln!("  workspace: {workspace}");
    eprintln!("  branch:    {branch_name}");
    eprintln!("  files:     {} modified", modified_files.len());
    for f in &modified_files {
        eprintln!("             {f}");
    }

    // Build the remote shell script. Do NOT stage via `git add .` — use the
    // recorded list, so concurrent unrelated edits on the worker don't leak in.
    let mut script = String::new();
    script.push_str(&format!("cd {workspace} && "));
    script.push_str(&format!(
        "git fetch origin main >/dev/null 2>&1 || true && \
         git checkout -b {shell_branch} 2>&1 && ",
        shell_branch = shell_quote(&branch_name)
    ));
    for f in &modified_files {
        script.push_str(&format!("git add -- {} && ", shell_quote(f)));
    }
    script.push_str(&format!(
        "git commit -m {msg} 2>&1",
        msg = shell_quote(&commit_msg)
    ));

    let target = format!("{ssh_user}@{primary_ip}");
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &target,
            &script,
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("ssh commit: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "remote git checkout/add/commit failed (rc={:?}):\n  stdout: {}\n  stderr: {}",
            out.status.code(),
            stdout.trim(),
            stderr.trim()
        ));
    }
    eprintln!("{GREEN}✓ committed{RESET}");

    // 4. Optional push.
    let should_push = push || pr;
    if should_push {
        let push_cmd = format!(
            "cd {workspace} && git push -u origin {br}",
            br = shell_quote(&branch_name)
        );
        let out = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                &push_cmd,
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("ssh push: {e}"))?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "remote git push failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        eprintln!("{GREEN}✓ pushed{RESET} origin/{branch_name}");
    }

    // 5. Optional PR via gh on the worker.
    let mut pr_url: Option<String> = None;
    if pr {
        // Confirm gh auth before attempting.
        let auth_check = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                "gh auth status >/dev/null 2>&1 && echo ok || echo missing",
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("ssh gh auth status: {e}"))?;
        let auth_ok = String::from_utf8_lossy(&auth_check.stdout).trim() == "ok";
        if !auth_ok {
            return Err(anyhow::anyhow!(
                "gh CLI is not authenticated on {worker}. \
                 Run `ssh {target} gh auth login` first, or skip --pr."
            ));
        }

        let body = format!(
            "Produced by ff agent on {worker} in session {session_id}.\n\n\
             - Worker: {worker}\n\
             - Model:  {}\n\
             - Tokens: prompt={} completion={}\n\
             - Files:  {} modified\n\n\
             Generated by `ff agent commit-back`.",
            model_id.as_deref().unwrap_or("(unknown)"),
            tok_in.unwrap_or(0),
            tok_out.unwrap_or(0),
            modified_files.len(),
        );
        let pr_title = title.as_deref().unwrap_or("ff agent commit-back");

        let gh_cmd = format!(
            "cd {workspace} && gh pr create --base main --head {br} \
             --title {title_q} --body {body_q}",
            br = shell_quote(&branch_name),
            title_q = shell_quote(pr_title),
            body_q = shell_quote(&body),
        );
        let out = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=30",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                &gh_cmd,
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("ssh gh pr create: {e}"))?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "remote `gh pr create` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !url.is_empty() {
            pr_url = Some(url.clone());
            eprintln!("{GREEN}✓ PR opened{RESET} {url}");
        } else {
            eprintln!("{YELLOW}! PR created but no URL returned{RESET}");
        }
    }

    // Persist branch + PR URL onto the work_item.
    let _ = sqlx::query(
        "UPDATE work_items SET branch_name = COALESCE(branch_name, $2), \
                                pr_url = COALESCE($3, pr_url) \
         WHERE id = $1",
    )
    .bind(work_item_id)
    .bind(&branch_name)
    .bind(pr_url.as_deref())
    .execute(pool)
    .await;

    // Best-effort NATS event.
    let payload = serde_json::json!({
        "session_id": session_id,
        "work_item_id": work_item_id,
        "worker": worker,
        "branch": branch_name,
        "pr_url": pr_url,
        "files": modified_files,
        "ts": now.to_rfc3339(),
    });
    ff_agent::nats_client::publish_json(
        "fleet.events.agent.commit_back_completed".to_string(),
        &payload,
    )
    .await;

    eprintln!();
    eprintln!("{GREEN}✓ ff agent commit-back complete{RESET}");
    if let Some(url) = pr_url {
        println!("{url}");
    } else {
        println!("{branch_name}");
    }
    Ok(())
}

/// Slugify a title for use in a git branch name: lowercase, ASCII-only,
/// non-alphanumerics collapsed to '-', max 40 chars.
pub fn slugify_for_branch(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(40));
    let mut prev_dash = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

/// Wrap a string as a single-quoted POSIX shell argument.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            // Close the quote, append an escaped apostrophe, reopen.
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}



pub async fn handle_agent(cmd: crate::AgentCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::AgentCommand::Seed => {
            let n = ff_agent::agent_coordinator::seed_slot_zero_for_all(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("seed: {e}"))?;
            println!("{GREEN}✓{RESET} seeded {n} new sub_agent row(s)");
            Ok(())
        }
        crate::AgentCommand::SubAgents { json } => {
            let rows = ff_agent::agent_coordinator::list_sub_agents(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("list: {e}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("(no sub_agent rows — run `ff agent seed`)");
                return Ok(());
            }
            println!(
                "{:<14} {:<4} {:<8} {:<36} WORKSPACE",
                "COMPUTER", "SLOT", "STATUS", "ID"
            );
            for r in rows {
                println!(
                    "{:<14} {:<4} {:<8} {:<36} {}",
                    r.computer,
                    r.slot,
                    r.status,
                    r.id.to_string(),
                    r.workspace_dir
                );
            }
            Ok(())
        }
        crate::AgentCommand::Dispatch {
            prompt,
            to_computer,
            work_item_id,
            json,
        } => {
            let wi_id = if let Some(id_str) = work_item_id.clone() {
                uuid::Uuid::parse_str(&id_str)
                    .map_err(|e| anyhow::anyhow!("invalid --work-item-id: {e}"))?
            } else {
                let created_by = ff_agent::fleet_info::resolve_this_node_name().await;
                ff_agent::agent_coordinator::create_transient_work_item(&pool, &prompt, &created_by)
                    .await
                    .map_err(|e| anyhow::anyhow!("create transient work_item: {e}"))?
            };

            let redis_url = resolve_pulse_redis_url();
            let reader = ff_pulse::reader::PulseReader::new(&redis_url)
                .map_err(|e| anyhow::anyhow!("pulse reader: {e}"))?;
            let coord = ff_agent::agent_coordinator::AgentCoordinator::new(
                pool.clone(),
                std::sync::Arc::new(reader),
            );

            let receipt = coord
                .dispatch_task(wi_id, prompt.clone(), to_computer.clone())
                .await
                .map_err(|e| anyhow::anyhow!("dispatch: {e}"))?;

            if json {
                let out = serde_json::json!({
                    "work_item_id": receipt.work_item_id,
                    "sub_agent_id": receipt.sub_agent_id,
                    "work_output_id": receipt.work_output_id,
                    "computer": receipt.computer_name,
                    "model": receipt.model_id,
                    "duration_ms": receipt.duration_ms,
                    "response": receipt.response_text,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("{GREEN}✓ dispatched{RESET}");
                println!("  work_item: {}", receipt.work_item_id);
                println!("  computer:  {}", receipt.computer_name);
                println!("  model:     {}", receipt.model_id);
                println!("  duration:  {}ms", receipt.duration_ms);
                if let Some(wo) = receipt.work_output_id {
                    println!("  output:    {wo}");
                }
                println!("\n{CYAN}── response ──{RESET}\n{}", receipt.response_text);
            }
            Ok(())
        }
        crate::AgentCommand::CommitBack { session, push, pr } => {
            handle_agent_commit_back(&pool, &session, push, pr).await
        }
        crate::AgentCommand::Fanout {
            prompt,
            backend,
            fanout,
        } => handle_agent_fanout(&pool, prompt, backend, fanout).await,
        crate::AgentCommand::DispatchEach { prompt, backend } => {
            handle_agent_dispatch_each(&pool, prompt, backend).await
        }
    }
}
