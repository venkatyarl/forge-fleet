use crate::whoami_tag;
use anyhow::Result;

pub async fn handle_defer(cmd: crate::DeferCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        crate::DeferCommand::List { status, limit } => {
            let rows = ff_db::pg_list_deferred(&pool, status.as_deref(), limit).await?;
            if rows.is_empty() {
                println!("(no deferred tasks)");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<12} {:<16} {:<6} TITLE",
                "ID", "STATUS", "TRIGGER", "TARGET", "TRY"
            );
            for r in rows {
                let trigger = (match r.trigger_type.as_str() {
                    "node_online" => r
                        .trigger_spec
                        .get("node")
                        .and_then(|v| v.as_str())
                        .map(|n| format!("node={n}"))
                        .unwrap_or_else(|| "node_online".into()),
                    "at_time" => r
                        .trigger_spec
                        .get("at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("at_time")
                        .to_string(),
                    other => other.to_string(),
                })
                .to_string();
                let target = r.preferred_node.clone().unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<10} {:<12} {:<16} {:<6} {}",
                    r.id,
                    r.status,
                    trigger,
                    target,
                    format!("{}/{}", r.attempts, r.max_attempts),
                    r.title
                );
            }
        }
        crate::DeferCommand::AddShell {
            title,
            run,
            when_node_online,
            when_at,
            on_node,
            max_attempts,
            max_duration_secs,
        } => {
            let (trigger_type, trigger_spec, preferred_node) =
                if let Some(node) = when_node_online.clone() {
                    (
                        "node_online".to_string(),
                        serde_json::json!({"node": node}),
                        on_node.clone().or(Some(node)),
                    )
                } else if let Some(at) = when_at {
                    (
                        "at_time".to_string(),
                        serde_json::json!({"at": at}),
                        on_node.clone(),
                    )
                } else {
                    anyhow::bail!("must specify --when-node-online <node> or --when-at <rfc3339>");
                };

            let mut payload = serde_json::json!({
                "command": run,
            });
            // Only set max_duration_secs when a positive cap is given; the
            // worker treats absent/0 as its DEFAULT_DEFER_MAX_DURATION (7200s).
            if let Some(secs) = max_duration_secs.filter(|s| *s > 0) {
                payload["max_duration_secs"] = serde_json::json!(secs);
            }
            let id = ff_db::pg_enqueue_deferred(
                &pool,
                &title,
                "shell",
                &payload,
                &trigger_type,
                &trigger_spec,
                preferred_node.as_deref(),
                &serde_json::json!([]),
                Some(&whoami_tag()),
                Some(max_attempts),
            )
            .await?;
            println!("Enqueued deferred task: {id}");
            println!("  title:         {title}");
            println!("  kind:          shell");
            println!("  trigger:       {trigger_type} ({trigger_spec})");
            if let Some(n) = &preferred_node {
                println!("  runs on node:  {n}");
            }
            println!("  max attempts:  {max_attempts}");
            match max_duration_secs.filter(|s| *s > 0) {
                Some(secs) => println!("  max duration:  {secs}s"),
                None => println!("  max duration:  7200s (worker default)"),
            }
            println!();
            println!(
                "Captured durably in Postgres. forgefleetd's defer-worker picks it up when the"
            );
            println!("      trigger fires; follow it with `ff defer get {id}`.");
        }
        crate::DeferCommand::Get { id } => match ff_db::pg_get_deferred(&pool, &id).await? {
            Some(r) => {
                println!("ID:            {}", r.id);
                println!("Title:         {}", r.title);
                println!("Status:        {}", r.status);
                println!("Kind:          {}", r.kind);
                println!("Trigger:       {} ({})", r.trigger_type, r.trigger_spec);
                println!(
                    "Preferred node:{}",
                    r.preferred_node.clone().unwrap_or_else(|| "-".into())
                );
                println!("Attempts:      {}/{}", r.attempts, r.max_attempts);
                println!(
                    "Created:       {}  by {}",
                    r.created_at.format("%Y-%m-%d %H:%M UTC"),
                    r.created_by.clone().unwrap_or_else(|| "-".into())
                );
                if let Some(ts) = r.next_attempt_at {
                    println!("Next attempt:  {}", ts.format("%Y-%m-%d %H:%M UTC"));
                }
                if let Some(n) = &r.claimed_by {
                    println!("Claimed by:    {n}");
                }
                if let Some(err) = &r.last_error {
                    println!("Last error:    {err}");
                }
                if let Some(res) = &r.result {
                    // Surface the full captured streams when present (shell
                    // tasks store {exit_code, stdout, stderr}); fall back to
                    // pretty JSON for other task kinds.
                    let stdout = res.get("stdout").and_then(|v| v.as_str());
                    let stderr = res.get("stderr").and_then(|v| v.as_str());
                    if stdout.is_some() || stderr.is_some() {
                        if let Some(code) = res.get("exit_code").and_then(|v| v.as_i64()) {
                            println!("Exit code:     {code}");
                        }
                        if let Some(s) = stdout.filter(|s| !s.is_empty()) {
                            println!("\n--- stdout ---\n{s}");
                        }
                        if let Some(s) = stderr.filter(|s| !s.is_empty()) {
                            println!("\n--- stderr ---\n{s}");
                        }
                    } else {
                        println!(
                            "Result:\n{}",
                            serde_json::to_string_pretty(res).unwrap_or_else(|_| res.to_string())
                        );
                    }
                }
                println!(
                    "\nPayload:\n{}",
                    serde_json::to_string_pretty(&r.payload).unwrap_or_default()
                );
            }
            None => {
                eprintln!("No deferred task with id '{id}'");
                std::process::exit(1);
            }
        },
        crate::DeferCommand::Cancel { id, force } => {
            let cancelled = if force {
                ff_db::pg_force_cancel_deferred(&pool, &id).await?
            } else {
                ff_db::pg_cancel_deferred(&pool, &id).await?
            };
            if cancelled {
                println!("Cancelled task {id}");
            } else if force {
                println!("Task {id} not cancellable (already terminal, or does not exist)");
            } else {
                println!(
                    "Task {id} is not in a cancellable state (use --force for a stuck 'running' task)"
                );
            }
        }
        crate::DeferCommand::Retry { id } => {
            if ff_db::pg_retry_deferred(&pool, &id).await? {
                println!("Task {id} requeued for retry (status=pending)");
            } else {
                println!("Task {id} is not in a retryable state (must be failed or cancelled)");
            }
        }
    }
    Ok(())
}
