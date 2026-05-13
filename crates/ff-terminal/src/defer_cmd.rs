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

            let payload = serde_json::json!({
                "command": run,
            });
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
            println!();
            println!(
                "NOTE: executor loop is not yet running. Task is captured durably in Postgres"
            );
            println!("      and will begin processing once `forgefleetd defer-worker` is live.");
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
                    println!("Result:        {res}");
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
        crate::DeferCommand::Cancel { id } => {
            if ff_db::pg_cancel_deferred(&pool, &id).await? {
                println!("Cancelled task {id}");
            } else {
                println!("Task {id} is not in a cancellable state (or does not exist)");
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
