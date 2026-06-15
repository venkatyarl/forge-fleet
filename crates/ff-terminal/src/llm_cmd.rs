use crate::truncate_for_col;
use anyhow::Result;

pub async fn handle_llm(cmd: crate::LlmCommand) -> Result<()> {
    match cmd {
        crate::LlmCommand::Status { json } => handle_llm_status(json).await,
    }
}

pub async fn handle_llm_status(json: bool) -> Result<()> {
    let reader = crate::pulse_reader()?;
    let mut servers = reader
        .list_llm_servers()
        .await
        .map_err(|e| anyhow::anyhow!("list_llm_servers: {e}"))?;

    let all_computers = reader.list_computers().await.unwrap_or_default();

    // Build a computer-name → primary_ip map from the pulse beats so every row
    // (servers AND the "(no server)" list) reads in subnet order, matching the
    // fleet-table convention shared by `ff fleet health`/`nodes`. `list_computers`
    // / `list_llm_servers` only carry names, so the IP comes from the beats.
    let ip_by_name: std::collections::HashMap<String, String> = reader
        .beats_by_name()
        .await
        .map(|m| {
            m.into_iter()
                .map(|(name, b)| (name, b.network.primary_ip))
                .collect()
        })
        .unwrap_or_default();
    let ip_rank = |name: &str| -> u32 {
        crate::helpers::ip_sort_key(ip_by_name.get(name).map(String::as_str).unwrap_or(""))
    };

    // Sort servers by primary IP (numeric octets), name as a stable tiebreak,
    // BEFORE both the JSON and text branches so they share one stable order.
    servers.sort_by(|(a, _), (b, _)| ip_rank(a).cmp(&ip_rank(b)).then_with(|| a.cmp(b)));

    // Each node reports its endpoint as the loopback host it sees locally; rewrite
    // to the node's primary IP so the printed endpoint is reachable from here.
    let reachable = |name: &str, endpoint: &str| -> String {
        crate::helpers::reachable_endpoint(
            endpoint,
            ip_by_name.get(name).map(String::as_str).unwrap_or(""),
        )
    };

    if json {
        let arr: Vec<_> = servers
            .iter()
            .map(|(computer, s)| {
                serde_json::json!({
                    "computer":  computer,
                    "model":     s.model.id,
                    "runtime":   s.runtime,
                    "endpoint":  reachable(computer, &s.endpoint),
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
            truncate_for_col(&reachable(computer, &s.endpoint), 32),
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
    // Same subnet order as the server rows (was alphabetical) — name tiebreak.
    missing.sort_by(|a, b| ip_rank(a).cmp(&ip_rank(b)).then_with(|| a.cmp(b)));
    for c in &missing {
        println!("{:<10} (no server)", truncate_for_col(c, 10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::helpers::ip_sort_key;
    use std::collections::HashMap;

    // Mirrors the ip_rank closure + the (ip, name) ordering used by both the
    // server table and the "(no server)" list in handle_llm_status.
    fn order(ip_by_name: &HashMap<&str, &str>, names: &[&str]) -> Vec<String> {
        let rank = |n: &str| ip_sort_key(ip_by_name.get(n).copied().unwrap_or(""));
        let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
        v
    }

    #[test]
    fn sorts_by_numeric_octet_not_alphabetical() {
        let map = HashMap::from([
            ("taylor", "192.168.5.100"),
            ("marcus", "192.168.5.102"),
            ("beyonce", "192.168.5.119"),
            ("ace", "192.168.5.9"),
        ]);
        // Alphabetical would put ace, beyonce, marcus, taylor; octet order is
        // .9 < .100 < .102 < .119 (and `.100` must NOT sort before `.9`).
        assert_eq!(
            order(&map, &["taylor", "beyonce", "marcus", "ace"]),
            vec!["ace", "taylor", "marcus", "beyonce"]
        );
    }

    #[test]
    fn unknown_ip_sorts_last_with_name_tiebreak() {
        let map = HashMap::from([("taylor", "192.168.5.100")]);
        // "ghost" / "zombie" have no beat → ip rank u32::MAX → sort last, and
        // among themselves fall back to the name tiebreak (ghost before zombie).
        assert_eq!(
            order(&map, &["zombie", "ghost", "taylor"]),
            vec!["taylor", "ghost", "zombie"]
        );
    }
}
