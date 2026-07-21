use crate::{CYAN, GREEN, RED, RESET, YELLOW, truncate_str};
use anyhow::Result;
use std::path::Path;

pub async fn handle_task(cmd: crate::TaskCommand, _config_path: &Path) -> Result<()> {
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(reqwest::Client::new);
    let client = &*SHARED_HTTP;
    let base = "http://127.0.0.1:50002";

    match cmd {
        crate::TaskCommand::List { status, limit } => {
            let resp = ff_agent::http_auth::send_signed_get(client, &format!("{base}/tasks")).await;
            let body = match resp {
                Ok(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
                Err(e) => {
                    println!(
                        "{RED}✗ Cannot reach agent HTTP server (is forgefleetd running?): {e}{RESET}"
                    );
                    return Ok(());
                }
            };

            let empty = vec![];
            let all_tasks = body
                .get("tasks")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty);
            let tasks: Vec<&serde_json::Value> = all_tasks
                .iter()
                .filter(|t| {
                    if let Some(ref s) = status {
                        t.get("status").and_then(|v| v.as_str()) == Some(s.as_str())
                    } else {
                        true
                    }
                })
                .take(limit as usize)
                .collect();

            if tasks.is_empty() {
                println!("{YELLOW}No tasks found{RESET}");
                return Ok(());
            }

            println!("{GREEN}✓ Tasks ({} shown){RESET}", tasks.len());
            println!(
                "  {:<6} {:<40} {:<12} {:<16} CREATED",
                "ID", "SUBJECT", "STATUS", "NODE"
            );
            println!("  {}", "-".repeat(95));
            for t in &tasks {
                let id = t.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                let subject = t.get("subject").and_then(|v| v.as_str()).unwrap_or("-");
                let status_str = t.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let node = t.get("origin_node").and_then(|v| v.as_str()).unwrap_or("-");
                let created = t.get("created_at").and_then(|v| v.as_str()).unwrap_or("-");
                let status_color = match status_str {
                    "completed" => GREEN,
                    "failed" => RED,
                    "in_progress" => CYAN,
                    _ => YELLOW,
                };
                let short_subject = truncate_str(subject, 39);
                let short_created = truncate_str(created, 19);
                println!(
                    "  {id:<6} {short_subject:<40} {status_color}{status_str:<12}{RESET} {node:<16} {short_created}"
                );
            }
        }
        crate::TaskCommand::Get { id } => {
            let resp = ff_agent::http_auth::send_signed_get(client, &format!("{base}/tasks")).await;
            let body = match resp {
                Ok(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
                Err(e) => {
                    println!("{RED}✗ Cannot reach agent HTTP server: {e}{RESET}");
                    return Ok(());
                }
            };

            let empty = vec![];
            let task = body
                .get("tasks")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty)
                .iter()
                .find(|t| {
                    t.get("id")
                        .and_then(|v| v.as_str())
                        .map(|tid| tid == id || tid.starts_with(&id))
                        .unwrap_or(false)
                });

            match task {
                None => println!("{RED}✗ Task not found: {id}{RESET}"),
                Some(t) => {
                    let tid = t.get("id").and_then(|v| v.as_str()).unwrap_or(&id);
                    let subject = t.get("subject").and_then(|v| v.as_str()).unwrap_or("-");
                    let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                    let node = t.get("origin_node").and_then(|v| v.as_str()).unwrap_or("-");
                    let created = t.get("created_at").and_then(|v| v.as_str()).unwrap_or("-");
                    println!("{GREEN}✓ Task #{tid}{RESET}");
                    println!("  subject:     {subject}");
                    println!("  status:      {status}");
                    println!("  origin_node: {node}");
                    println!("  created:     {created}");
                    if let Some(output) = t.get("output").and_then(|v| v.as_str())
                        && !output.is_empty()
                    {
                        println!("\n  Output:\n    {}", truncate_str(output, 500));
                    }
                }
            }
        }
        crate::TaskCommand::Update { id, status } => {
            let valid = ["pending", "in_progress", "completed", "failed", "cancelled"];
            if !valid.contains(&status.as_str()) {
                println!(
                    "{RED}✗ Invalid status '{status}'. Valid: {}{RESET}",
                    valid.join(", ")
                );
                return Ok(());
            }
            let payload = serde_json::json!({
                "task_id": id,
                "status": status,
                "output": "",
                "from": "ff-cli",
            });
            let r = ff_agent::http_auth::send_signed_json(
                client,
                &format!("{base}/agent/message"),
                &payload,
            )
            .await;
            match r {
                Ok(_) => println!("{GREEN}✓ Task #{id} → {status}{RESET}"),
                Err(e) => println!("{RED}✗ Failed: {e}{RESET}"),
            }
        }
    }
    Ok(())
}
