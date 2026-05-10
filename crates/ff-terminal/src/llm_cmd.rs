use anyhow::Result;
use crate::truncate_for_col;

pub async fn handle_llm(cmd: crate::LlmCommand) -> Result<()> {
    match cmd {
        crate::LlmCommand::Status { json } => handle_llm_status(json).await,
    }
}

pub async fn handle_llm_status(json: bool) -> Result<()> {
    let reader = crate::pulse_reader()?;
    let servers = reader
        .list_llm_servers()
        .await
        .map_err(|e| anyhow::anyhow!("list_llm_servers: {e}"))?;

    let all_computers = reader.list_computers().await.unwrap_or_default();

    if json {
        let arr: Vec<_> = servers
            .iter()
            .map(|(computer, s)| {
                serde_json::json!({
                    "computer":  computer,
                    "model":     s.model.id,
                    "runtime":   s.runtime,
                    "endpoint":  s.endpoint,
                    "queue_depth": s.queue_depth,
                    "active_requests": s.active_requests,
                    "tokens_per_sec_last_min": s.tokens_per_sec_last_min,
                    "is_healthy": s.is_healthy,
                    "status":    s.status,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if servers.is_empty() {
        println!("(no running LLM servers)");
        if !all_computers.is_empty() {
            println!("computers present in pulse: {}", all_computers.join(", "));
        }
        return Ok(());
    }

    println!(
        "{:<10} {:<20} {:<10} {:<32} {:<5} {:<6} {:<7} {:<8}",
        "COMPUTER", "MODEL", "RUNTIME", "ENDPOINT", "QUEUE", "ACTIVE", "TOK/S", "HEALTH"
    );
    for (computer, s) in &servers {
        let health = if s.is_healthy { "healthy" } else { "unhealthy" };
        println!(
            "{:<10} {:<20} {:<10} {:<32} {:<5} {:<6} {:<7.1} {:<8}",
            truncate_for_col(computer, 10),
            truncate_for_col(&s.model.id, 20),
            truncate_for_col(&s.runtime, 10),
            truncate_for_col(&s.endpoint, 32),
            s.queue_depth,
            s.active_requests,
            s.tokens_per_sec_last_min,
            health
        );
    }

    let hosts_with_server: std::collections::HashSet<&str> =
        servers.iter().map(|(c, _)| c.as_str()).collect();
    let mut missing: Vec<&String> = all_computers
        .iter()
        .filter(|c| !hosts_with_server.contains(c.as_str()))
        .collect();
    missing.sort();
    for c in &missing {
        println!("{:<10} (no server)", truncate_for_col(c, 10));
    }
    Ok(())
}
