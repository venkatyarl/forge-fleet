//! `ff agent` subcommand implementations.

use anyhow::Result;

use crate::{CYAN, GREEN, RESET, resolve_pulse_redis_url};

/// GAP-D-iso: treat `run_cwd` as the canonical repo, then run each dispatched
/// agent in a fresh throwaway worktree. The worktree intentionally persists
/// after the run so commit-back can lift changes from the recorded working_dir.
fn isolated_worktree_run_command(
    run_cwd: &str,
    remote: &str,
    base_branch: &str,
    backend: &str,
    timeout: u64,
    shell_safe_prompt: &str,
) -> String {
    let remote_ref = format!("{remote}/{base_branch}");
    format!(
        "CANON={canon_q}; RUNS=\"$HOME/.forgefleet/runs\"; mkdir -p \"$RUNS\"; \
         WT=\"$RUNS/run-$(date +%s%N)-$$\"; \
         if git -C \"$CANON\" rev-parse --git-dir >/dev/null 2>&1; then \
         git -C \"$CANON\" fetch {remote_q} --quiet 2>/dev/null || true; \
         git -C \"$CANON\" worktree add --detach --force \"$WT\" {remote_ref_q} >/dev/null 2>&1 || \
         git -C \"$CANON\" worktree add --detach --force \"$WT\" >/dev/null 2>&1 || WT=\"$CANON\"; \
         else WT=\"$CANON\"; fi; \
         ff run --backend {backend} --cwd \"$WT\" --timeout {timeout} '{shell_safe_prompt}'",
        canon_q = shell_quote(run_cwd),
        remote_q = shell_quote(remote),
        remote_ref_q = shell_quote(&remote_ref),
    )
}

pub fn prune_stale_run_worktrees(max_age_hours: u64) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let runs_dir = std::path::PathBuf::from(home).join(".forgefleet/runs");
    let Ok(entries) = std::fs::read_dir(&runs_dir) else {
        return;
    };
    let max_age = std::time::Duration::from_secs(max_age_hours.saturating_mul(60 * 60));
    let now = std::time::SystemTime::now();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("run-") || !path.is_dir() {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age > max_age {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[derive(Debug, Clone)]
struct AgentGitPolicy {
    base_branch: String,
    integration_strategy: String,
    branch_prefix: String,
    git_remote: String,
}

async fn resolve_agent_git_policy(
    pool: &sqlx::PgPool,
    project: Option<&str>,
) -> Result<AgentGitPolicy> {
    let Some(project_id) = project else {
        return Ok(AgentGitPolicy {
            base_branch: "main".to_string(),
            integration_strategy: "feature_pr".to_string(),
            branch_prefix: "fleet".to_string(),
            git_remote: "origin".to_string(),
        });
    };

    let policy = ff_db::pg_get_project_git_policy(pool, project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("project '{project_id}' not found"))?;

    let integration_strategy = policy.integration_strategy.trim().to_string();
    match integration_strategy.as_str() {
        "direct" | "feature_pr" | "feature_push" => {}
        other => {
            return Err(anyhow::anyhow!(
                "project '{}' has unsupported integration_strategy '{}'",
                policy.id,
                other
            ));
        }
    }

    Ok(AgentGitPolicy {
        base_branch: non_empty_or(policy.default_branch.trim(), "main"),
        integration_strategy,
        branch_prefix: non_empty_or(policy.branch_prefix.trim().trim_matches('/'), "fleet"),
        git_remote: non_empty_or(policy.git_remote.trim(), "origin"),
    })
}

fn non_empty_or(value: &str, default: &str) -> String {
    if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    }
}

const REMOTE_GIT_ENV: &str = "env -i PATH=/usr/bin:/bin HOME=/nonexistent \
GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_GLOBAL=/dev/null \
GIT_CONFIG_LOCAL=/dev/null GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false \
SSH_ASKPASS=/bin/false GIT_SSH_COMMAND=/bin/false";
const REMOTE_GIT: &str = "git -c core.hooksPath=/dev/null \
-c core.attributesFile=/dev/null -c credential.helper= \
-c protocol.file.allow=never -c protocol.ext.allow=never";

#[derive(Debug)]
struct CommitBackAuthority {
    worker: String,
    ssh_user: String,
    primary_ip: String,
    slot: i32,
    workspace: String,
    source_root: String,
    repo_url: String,
}

fn has_unsafe_path_char(value: &str) -> bool {
    value.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '\\' | '\0'
            )
    })
}

fn validate_absolute_location<'a>(label: &str, value: &'a str) -> Result<&'a std::path::Path> {
    use std::path::{Component, Path};

    if value.is_empty() || value.starts_with('~') || has_unsafe_path_char(value) {
        anyhow::bail!("{label} contains unsupported characters");
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        anyhow::bail!("{label} must be absolute");
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        anyhow::bail!("{label} contains traversal");
    }
    Ok(path)
}

fn validate_commit_back_locations(
    metadata: &serde_json::Value,
    modified_files: &[String],
    authority: &CommitBackAuthority,
) -> Result<Vec<String>> {
    let workspace = validate_absolute_location("registered workspace", &authority.workspace)?;
    let source_root = validate_absolute_location("registered source root", &authority.source_root)?;
    let supplied_workspace = metadata
        .get("working_dir")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("work output has no absolute metadata.working_dir"))?;
    validate_absolute_location("metadata.working_dir", supplied_workspace)?;
    if supplied_workspace != authority.workspace {
        anyhow::bail!("metadata.working_dir does not match the leased slot workspace");
    }

    let expected_slot = format!("sub-agent-{}", authority.slot);
    if !workspace
        .components()
        .any(|part| part.as_os_str() == expected_slot.as_str())
    {
        anyhow::bail!("registered workspace does not match the leased slot");
    }
    if !source_root.is_absolute() {
        anyhow::bail!("registered source root must be absolute");
    }

    modified_files
        .iter()
        .map(|file| {
            let absolute = validate_absolute_location("modified_files entry", file)?;
            let relative = absolute.strip_prefix(workspace).map_err(|_| {
                anyhow::anyhow!("modified_files entry is outside the leased slot workspace")
            })?;
            if relative.as_os_str().is_empty() {
                anyhow::bail!("modified_files entry names the workspace itself");
            }
            Ok(relative.to_string_lossy().into_owned())
        })
        .collect()
}

fn validate_git_atom(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.contains("..")
        || value.contains("@{")
        || value.ends_with('.')
        || value.ends_with('/')
        || value.ends_with(".lock")
        || has_unsafe_path_char(value)
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        anyhow::bail!("{label} is not a safe Git name");
    }
    Ok(())
}

fn remote_commit_script(
    authority: &CommitBackAuthority,
    branch_name: &str,
    commit_msg: &str,
    modified_files: &[String],
) -> String {
    let slot_root = std::path::Path::new(&authority.workspace)
        .parent()
        .expect("validated workspace has a parent")
        .to_string_lossy();
    let mut args = vec![
        authority.workspace.as_str(),
        slot_root.as_ref(),
        authority.source_root.as_str(),
        authority.repo_url.as_str(),
        branch_name,
        commit_msg,
    ];
    args.extend(modified_files.iter().map(String::as_str));

    let body = format!(
        "set -eu; workspace=$1; slot_root=$2; source_root=$3; repo_url=$4; \
         branch=$5; message=$6; shift 6; \
         workspace_real=$(realpath -e -- \"$workspace\"); \
         slot_real=$(realpath -e -- \"$slot_root\"); \
         source_real=$(realpath -e -- \"$source_root\"); \
         case \"$workspace_real/\" in \"$slot_real/\"*|\"$source_real/\"*) ;; \
           *) echo 'workspace escapes approved roots' >&2; exit 70;; esac; \
         test \"$({git_env} {git} -C \"$workspace_real\" rev-parse --show-toplevel)\" = \"$workspace_real\"; \
         test \"$({git_env} {git} -C \"$workspace_real\" remote get-url origin)\" = \"$repo_url\"; \
         orig=$({git_env} {git} -C \"$workspace_real\" symbolic-ref --quiet --short HEAD || \
              {git_env} {git} -C \"$workspace_real\" rev-parse HEAD); \
         {git_env} {git} -C \"$workspace_real\" check-ref-format --branch \"$branch\" >/dev/null; \
         {git_env} {git} -C \"$workspace_real\" checkout -b \"$branch\"; \
         for file do \
           file_real=$(realpath -e -- \"$workspace_real/$file\"); \
           case \"$file_real\" in \"$workspace_real\"/*) ;; \
             *) echo 'modified file escapes workspace' >&2; exit 71;; esac; \
           {git_env} {git} -C \"$workspace_real\" add -- \"$file\"; \
         done; \
         {git_env} {git} -C \"$workspace_real\" commit -m \"$message\"; \
         {git_env} {git} -C \"$workspace_real\" checkout --detach \"$orig\"",
        git_env = REMOTE_GIT_ENV,
        git = REMOTE_GIT,
    );
    let mut command = format!("sh -c {} sh", shell_quote(&body));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

pub async fn handle_agent_fanout(
    pool: &sqlx::PgPool,
    prompt: String,
    backend: String,
    fanout: u32,
    cwd: Option<String>,
    timeout: u64,
    project: Option<String>,
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
    // GAP-D1-fanout: target a controlled checkout so the dispatched run records
    // its working_dir there and `ff agent commit-back` can lift it. Default to
    // the member's fleet forge-fleet checkout.
    let run_cwd = cwd
        .clone()
        .unwrap_or_else(|| "~/.forgefleet/sub-agents/sub-agent-0/forge-fleet".to_string());
    let git_policy = resolve_agent_git_policy(pool, project.as_deref()).await?;
    // Pass --timeout to the dispatched run (bounds the CLI subprocess) AND give
    // the fleet task a matching max_duration_secs (worker cap) with a small
    // buffer, so a multi-minute codex/kimi build isn't killed at the 600s
    // default by EITHER cap. The CLI --timeout fires first (checkpoint), the
    // worker is the backstop.
    let cmd = isolated_worktree_run_command(
        &run_cwd,
        &git_policy.git_remote,
        &git_policy.base_branch,
        cfg.name,
        timeout,
        &shell_safe_prompt,
    );
    let task_max_secs = timeout + 120;
    for i in 0..fanout {
        ff_agent::task_runner::pg_enqueue_shell_task_full(
            pool,
            &format!("agent-fanout/{i}: {} backend={}", cfg.name, cfg.name),
            &cmd,
            &[cfg.name.to_string()],
            None,
            Some(parent),
            70,
            leader_computer_id,
            false,
            &[],
            Some(task_max_secs),
            None,
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
    cwd: Option<String>,
    timeout: u64,
    project: Option<String>,
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
    let run_cwd = cwd
        .clone()
        .unwrap_or_else(|| "~/.forgefleet/sub-agents/sub-agent-0/forge-fleet".to_string());
    let git_policy = resolve_agent_git_policy(pool, project.as_deref()).await?;
    // See handle_agent_fanout: --timeout bounds the CLI, task max_duration_secs
    // bounds the worker; both raised above the 600s default for build runs.
    let cmd = isolated_worktree_run_command(
        &run_cwd,
        &git_policy.git_remote,
        &git_policy.base_branch,
        cfg.name,
        timeout,
        &shell_safe_prompt,
    );
    let task_max_secs = timeout + 120;
    for (_id, name) in &members {
        ff_agent::task_runner::pg_enqueue_shell_task_full(
            pool,
            &format!("agent-dispatch-each: {} on {}", cfg.name, name),
            &cmd,
            &[cfg.name.to_string()],
            Some(name),
            Some(parent),
            70,
            leader_computer_id,
            false,
            &[],
            Some(task_max_secs),
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("enqueue task on {name}: {e}"))?;
    }

    println!("composed parent task: {parent}");
    println!("watch progress with: ff tasks list --status pending,running --show-id");
    Ok(())
}

// ─── #118: ff agent commit-back — fleet-LLM work → git integration ─────────
//
// Lifts code produced by a fleet LLM in a sub-agent workspace back to Taylor's
// canonical repo via a feature branch + (optional) PR, or a project-policy
// direct push.
//
// Flow:
//   1. Look up `work_outputs` WHERE agent_session_id = <session>. Pick the
//      latest row. Extract `produced_on_computer`, `modified_files`, title.
//   2. Resolve the worker's ssh_user + primary_ip from `fleet_workers`.
//      Resolve the canonical source-tree path via `software_registry.install_path`
//      (falls back to `~/.forgefleet/sub-agents/sub-agent-0/forge-fleet` per convention).
//   3. SSH into the worker and run git checkout -b / add / commit / (push / gh pr create).
//   4. Persist the resulting branch + PR URL back into `work_items.pr_url`
//      (via the work_item linked to the work_output).
//   5. Best-effort publish `fleet.events.agent.commit_back_completed` on NATS.
pub async fn handle_agent_commit_back(
    pool: &sqlx::PgPool,
    session_id: &str,
    push: bool,
    pr: bool,
    project: Option<String>,
) -> Result<()> {
    use tokio::process::Command;
    prune_stale_run_worktrees(24);

    let project_supplied = project.is_some();
    let git_policy = resolve_agent_git_policy(pool, project.as_deref()).await?;
    if push || pr || project_supplied {
        anyhow::bail!(
            "agent commit-back push and PR integration is disabled until the typed Git transport boundary is available"
        );
    }
    validate_git_atom("branch prefix", &git_policy.branch_prefix)?;
    validate_git_atom("base branch", &git_policy.base_branch)?;
    validate_git_atom("Git remote", &git_policy.git_remote)?;

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
        serde_json::Value, // metadata
    )> = sqlx::query_as(
        "SELECT id, work_item_id, title, produced_on_computer, modified_files, \
                llm_model_id, llm_tokens_input, llm_tokens_output, metadata \
         FROM work_outputs \
         WHERE agent_session_id = $1 \
         ORDER BY produced_at DESC \
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query work_outputs: {e}"))?;

    let (
        wo_id,
        work_item_id,
        title,
        worker,
        modified_files_json,
        _model_id,
        _tok_in,
        _tok_out,
        metadata,
    ) = row.ok_or_else(|| {
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

    // 2. Resolve the workspace only through the exact work-item lease, slot,
    // repository, and computer registrations. The work output merely asserts
    // identity; it never supplies authority.
    let authority_row: Option<(String, String, String, i32, String, String, String)> =
        sqlx::query_as(
            "SELECT c.name, c.ssh_user, c.primary_ip, sa.slot, sa.workspace_dir, \
                    c.source_tree_path, COALESCE(pr.github_url, wi.repo_url) \
             FROM work_items wi \
             JOIN work_item_leases wl ON wl.work_item_id = wi.id \
             JOIN sub_agents sa ON sa.id = wl.sub_agent_id \
                               AND sa.computer_id = wl.computer_id \
                               AND sa.current_work_item_id = wi.id \
             JOIN computers c ON c.id = wl.computer_id \
             LEFT JOIN project_repos pr ON pr.id = wi.repo_id \
             WHERE wi.id = $1 \
               AND wl.session_id::text = $2 \
               AND wl.lease_state = 'building' \
               AND wl.lease_expires_at > NOW() \
               AND wi.assigned_computer = c.name \
               AND c.source_tree_path IS NOT NULL \
               AND COALESCE(pr.github_url, wi.repo_url) IS NOT NULL \
             ORDER BY wl.attempt DESC \
             LIMIT 1",
        )
        .bind(work_item_id)
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("resolve exact commit-back authority: {e}"))?;
    let (registered_worker, ssh_user, primary_ip, slot, workspace, source_root, repo_url) =
        authority_row.ok_or_else(|| {
            anyhow::anyhow!(
                "no active exact lease/slot/repo authority for work item {work_item_id} and session {session_id}"
            )
        })?;
    if worker != registered_worker {
        anyhow::bail!(
            "work output computer does not match exact leased computer ({worker} != {registered_worker})"
        );
    }
    let authority = CommitBackAuthority {
        worker: registered_worker,
        ssh_user,
        primary_ip,
        slot,
        workspace,
        source_root,
        repo_url,
    };
    let modified_files = validate_commit_back_locations(&metadata, &modified_files, &authority)?;

    // 3. Build branch name: <branch_prefix>/<worker>/<yyyymmdd-HHMMSS>-<slug>-<wi8>.
    //    The work_item_id suffix (GAP-B) guarantees uniqueness even when two
    //    commit-backs land in the same second on the same worker with the same
    //    title — otherwise `git checkout -b` collides under concurrent dispatch.
    let now = chrono::Utc::now();
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let title_slug = slugify_for_branch(title.as_deref().unwrap_or("agent-session"));
    let wi_short = work_item_id.simple().to_string();
    let branch_name = format!(
        "{}/{}/{stamp}-{title_slug}-{}",
        git_policy.branch_prefix,
        authority.worker,
        &wi_short[..8.min(wi_short.len())]
    );
    validate_git_atom("generated branch", &branch_name)?;
    let output_branch_name = if project_supplied && git_policy.integration_strategy == "direct" {
        git_policy.base_branch.clone()
    } else {
        branch_name.clone()
    };

    let commit_msg = format!(
        "{}\n\nProduced by ff agent on {worker} in session {session_id}.\n\n\
         Co-Authored-By: ForgeFleet Agent <agent@forgefleet.local>",
        title.as_deref().unwrap_or("ff agent commit-back")
    );

    eprintln!("{CYAN}▶ ff agent commit-back{RESET}");
    eprintln!("  session:   {session_id}");
    eprintln!(
        "  worker:    {} ({}@{})",
        authority.worker, authority.ssh_user, authority.primary_ip
    );
    eprintln!("  workspace: {}", authority.workspace);
    if let Some(project_id) = project.as_deref() {
        eprintln!("  project:   {project_id}");
        eprintln!("  policy:    {}", git_policy.integration_strategy);
        eprintln!(
            "  base:      {}/{}",
            git_policy.git_remote, git_policy.base_branch
        );
    }
    eprintln!("  branch:    {output_branch_name}");
    eprintln!("  files:     {} modified", modified_files.len());
    for f in &modified_files {
        eprintln!("             {f}");
    }

    // Build the remote shell script. Do NOT stage via `git add .` — use the
    // recorded list, so concurrent unrelated edits on the worker don't leak in.
    //
    // GAP-D-collision: capture the workspace's current branch first and restore
    // it after committing, so commit-back NEVER leaves a shared/live checkout
    // (e.g. taylor's `~/projects/forge-fleet` dev tree) switched onto the fleet
    // branch — observed switching the operator's working tree mid-session. The
    // commit lives on the new branch ref; the push/PR steps below operate on it
    // by name without it being checked out.
    let script = remote_commit_script(&authority, &branch_name, &commit_msg, &modified_files);
    let target = format!("{}@{}", authority.ssh_user, authority.primary_ip);
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "--",
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
    let pr_url: Option<String> = None;

    // Persist branch + PR URL onto the work_item.
    let _ = sqlx::query(
        "UPDATE work_items SET branch_name = COALESCE(branch_name, $2), \
                                pr_url = COALESCE($3, pr_url) \
         WHERE id = $1",
    )
    .bind(work_item_id)
    .bind(&output_branch_name)
    .bind(pr_url.as_deref())
    .execute(pool)
    .await;

    // Best-effort NATS event.
    let payload = serde_json::json!({
        "session_id": session_id,
        "work_item_id": work_item_id,
        "worker": worker,
        "branch": output_branch_name,
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
        println!("{output_branch_name}");
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

#[cfg(test)]
mod commit_back_security_tests {
    use super::*;

    fn authority() -> CommitBackAuthority {
        CommitBackAuthority {
            worker: "sophie".into(),
            ssh_user: "sophie".into(),
            primary_ip: "10.0.0.8".into(),
            slot: 0,
            workspace: "/home/sophie/.forgefleet/sub-agents/sub-agent-0/forge-fleet".into(),
            source_root: "/home/sophie/projects/forge-fleet".into(),
            repo_url: "git@github.com-venkat:venkatyarl/forge-fleet.git".into(),
        }
    }

    fn metadata(path: &str) -> serde_json::Value {
        serde_json::json!({ "working_dir": path })
    }

    #[test]
    fn hostile_workspace_metadata_is_rejected() {
        let auth = authority();
        let file = format!("{}/src/lib.rs", auth.workspace);
        for hostile in [
            "~/.forgefleet/sub-agents/sub-agent-0/forge-fleet",
            "relative/forge-fleet",
            "/home/sophie/other",
            "/home/sophie/.forgefleet/sub-agents/sub-agent-0/../outside",
            "/home/sophie/forge fleet",
            "/home/sophie/forge'fleet",
            "/home/sophie/forge;touch-pwned",
            "/home/sophie/forge$(touch-pwned)",
        ] {
            assert!(
                validate_commit_back_locations(
                    &metadata(hostile),
                    std::slice::from_ref(&file),
                    &auth
                )
                .is_err(),
                "accepted hostile workspace {hostile:?}"
            );
        }
    }

    #[test]
    fn hostile_modified_files_are_rejected() {
        let auth = authority();
        for hostile in [
            "src/lib.rs",
            "~/src/lib.rs",
            "/tmp/outside.rs",
            "/home/sophie/.forgefleet/sub-agents/sub-agent-0/forge-fleet/../outside.rs",
            "/home/sophie/.forgefleet/sub-agents/sub-agent-0/forge-fleet/white space.rs",
            "/home/sophie/.forgefleet/sub-agents/sub-agent-0/forge-fleet/quote'.rs",
            "/home/sophie/.forgefleet/sub-agents/sub-agent-0/forge-fleet/x;touch-pwned",
            "/home/sophie/.forgefleet/sub-agents/sub-agent-0/forge-fleet/$(touch-pwned)",
        ] {
            assert!(
                validate_commit_back_locations(
                    &metadata(&auth.workspace),
                    &[hostile.to_string()],
                    &auth
                )
                .is_err(),
                "accepted hostile file {hostile:?}"
            );
        }
    }

    #[test]
    fn valid_files_become_workspace_relative() {
        let auth = authority();
        let files = vec![
            format!("{}/crates/ff-terminal/src/agent_cmd.rs", auth.workspace),
            format!("{}/README.md", auth.workspace),
        ];
        assert_eq!(
            validate_commit_back_locations(&metadata(&auth.workspace), &files, &auth).unwrap(),
            ["crates/ff-terminal/src/agent_cmd.rs", "README.md"]
        );
    }

    #[test]
    fn branch_and_ref_validation_rejects_options_and_ref_syntax() {
        for hostile in [
            "-main",
            "refs/heads/x..y",
            "refs/heads/x@{0}",
            "refs/heads/x.lock",
            "refs/heads/x;",
            "refs/heads/$(touch-pwned)",
            "refs/heads/white space",
            "refs/heads/quote'",
        ] {
            assert!(validate_git_atom("ref", hostile).is_err(), "{hostile}");
        }
        assert!(validate_git_atom("ref", "fleet/sophie/safe-123").is_ok());
    }

    #[test]
    fn remote_script_canonicalizes_symlinks_and_sanitizes_git() {
        let auth = authority();
        let script = remote_commit_script(
            &auth,
            "fleet/sophie/safe-123",
            "safe commit",
            &["crates/ff-terminal/src/agent_cmd.rs".into()],
        );
        assert!(script.contains("realpath -e"));
        assert!(script.contains("workspace escapes approved roots"));
        assert!(script.contains("modified file escapes workspace"));
        assert!(script.contains("GIT_CONFIG_NOSYSTEM=1"));
        assert!(script.contains("GIT_CONFIG_SYSTEM=/dev/null"));
        assert!(script.contains("GIT_CONFIG_GLOBAL=/dev/null"));
        assert!(script.contains("GIT_CONFIG_LOCAL=/dev/null"));
        assert!(script.contains("core.hooksPath=/dev/null"));
        assert!(script.contains("GIT_SSH_COMMAND=/bin/false"));
        assert!(!script.contains("git push"));
        assert!(!script.contains("gh "));
    }
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
                let created_by = ff_agent::fleet_info::resolve_this_worker_name().await;
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
        crate::AgentCommand::CommitBack {
            session,
            push,
            pr,
            project,
        } => handle_agent_commit_back(&pool, &session, push, pr, project).await,
        crate::AgentCommand::Fanout {
            prompt,
            backend,
            fanout,
            run_cwd,
            timeout,
            project,
        } => handle_agent_fanout(&pool, prompt, backend, fanout, run_cwd, timeout, project).await,
        crate::AgentCommand::DispatchEach {
            prompt,
            backend,
            run_cwd,
            timeout,
            project,
        } => handle_agent_dispatch_each(&pool, prompt, backend, run_cwd, timeout, project).await,
    }
}
