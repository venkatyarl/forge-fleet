//! Data slash commands — pull live dashboard data from the gateway and format
//! it for inline display in the chat view.

use crate::app::App;

/// Run a data slash command. Returns formatted output if the command was handled.
pub async fn run(app: &App, cmd: &str) -> Option<String> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let name = parts.first()?;
    match *name {
        "/fleet" | "/nodes" => Some(render_fleet(app).await),
        "/models" => Some(render_models(app).await),
        "/tools" => Some(render_tools(app).await),
        "/alerts" => Some(render_alerts(app).await),
        "/interactions" => {
            let limit = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
            Some(render_interactions(app, limit).await)
        }
        "/settings" => Some(render_settings(app).await),
        "/config" => Some(render_config(app).await),
        "/skills" => Some(render_skills(app).await),
        "/brain" => Some(render_brain(app).await),
        "/status" => Some(render_status(app).await),
        _ => None,
    }
}

fn fmt_ts(raw: &str) -> String {
    if raw.is_empty() {
        return "-".to_string();
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| raw.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

async fn render_fleet(app: &App) -> String {
    let mut lines = vec!["Fleet status".to_string(), "─".repeat(40)];

    match app.gateway.get_fleet_status().await {
        Ok(status) => {
            let summary = status.summary.unwrap_or_default();
            lines.push(format!(
                "Total nodes: {}  Connected: {}  Unhealthy: {}",
                summary.total_nodes.unwrap_or(0),
                summary.connected_nodes.unwrap_or(0),
                summary.unhealthy_nodes.unwrap_or(0),
            ));
            lines.push(format!(
                "Models: {}  Leader: {}",
                summary.model_count.unwrap_or(0),
                summary.leader.as_deref().unwrap_or("unknown")
            ));
        }
        Err(e) => lines.push(format!("Status error: {e}")),
    }

    lines.push(String::new());
    lines.push("Computers".to_string());
    lines.push("─".repeat(40));

    match app.gateway.get_fleet_computers().await {
        Ok(nodes) => {
            if nodes.is_empty() {
                lines.push("No computers found.".to_string());
            } else {
                for n in nodes {
                    let status = n
                        .status
                        .as_deref()
                        .or(n.health.as_deref())
                        .unwrap_or("unknown");
                    let role = n.role.as_deref().unwrap_or("worker");
                    let ip = n.ip.as_deref().unwrap_or("-");
                    let models = n
                        .models_loaded
                        .as_ref()
                        .map(|m| m.join(", "))
                        .unwrap_or_default();
                    lines.push(format!(
                        "[{status:10}] {name:16} {role:10} {ip:15} {models}",
                        status = status,
                        name = n.name,
                        role = role,
                        ip = ip,
                        models = truncate(&models, 40)
                    ));
                }
            }
        }
        Err(e) => lines.push(format!("Computers error: {e}")),
    }

    lines.join("\n")
}

async fn render_models(app: &App) -> String {
    let mut lines = vec!["LLM servers".to_string(), "─".repeat(50)];
    match app.gateway.get_llm_servers().await {
        Ok(servers) => {
            if servers.is_empty() {
                lines.push("No LLM servers registered.".to_string());
            } else {
                lines.push(format!(
                    "{status:<8} {model:<28} {runtime:<10} {node:<12} {throughput:<10} {queue:<6}",
                    status = "STATUS",
                    model = "MODEL",
                    runtime = "RUNTIME",
                    node = "NODE",
                    throughput = "TOK/S",
                    queue = "QUEUE"
                ));
                for s in servers {
                    let status = if s.healthy { "online" } else { "offline" };
                    let enabled = if s.enabled.unwrap_or(true) {
                        ""
                    } else {
                        " (disabled)"
                    };
                    lines.push(format!(
                        "{status:<8} {model:<28} {runtime:<10} {node:<12} {throughput:<10.1} {queue:<6}{enabled}",
                        status = status,
                        model = truncate(&s.model, 28),
                        runtime = s.runtime,
                        node = truncate(&s.computer, 12),
                        throughput = s.tokens_per_sec,
                        queue = s.queue_depth,
                        enabled = enabled
                    ));
                }
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_tools(app: &App) -> String {
    let mut lines = vec!["Tool registry".to_string(), "─".repeat(50)];
    match app.gateway.get_tools().await {
        Ok(tools) => {
            if tools.is_empty() {
                lines.push("No tools registered.".to_string());
            } else {
                lines.push(format!(
                    "{health:<7} {tool:<24} {node:<12} {calls:<8} {latency:<12}",
                    health = "HEALTH",
                    tool = "TOOL",
                    node = "NODE",
                    calls = "CALLS",
                    latency = "AVG LATENCY"
                ));
                for t in tools {
                    let health = if t.healthy { "ok" } else { "crit" };
                    let latency = t
                        .avg_latency_ms
                        .map(|v| format!("{:.0} ms", v))
                        .unwrap_or_else(|| "-".to_string());
                    lines.push(format!(
                        "{health:<7} {tool:<24} {node:<12} {calls:<8} {latency:<12}",
                        health = health,
                        tool = truncate(&t.tool_name, 24),
                        node = truncate(&t.worker_name, 12),
                        calls = t.call_count,
                        latency = latency
                    ));
                }
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_alerts(app: &App) -> String {
    let mut lines = vec!["Alert events".to_string(), "─".repeat(50)];
    match app.gateway.get_alert_events().await {
        Ok(events) => {
            if events.is_empty() {
                lines.push("No alert events.".to_string());
            } else {
                for e in events.iter().take(20) {
                    let resolved = if e.resolved_at.is_some() {
                        "[RESOLVED]"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "[{severity}] {policy} ({metric}) {resolved}",
                        severity = e.severity,
                        policy = e.policy_name,
                        metric = e.metric,
                        resolved = resolved
                    ));
                    let mut meta = format!("  fired: {}", fmt_ts(&e.fired_at));
                    if let Some(name) = &e.computer_name {
                        meta.push_str(&format!(" · node: {name}"));
                    }
                    if let Some(msg) = &e.message {
                        meta.push_str(&format!(" · {msg}"));
                    }
                    lines.push(meta);
                }
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_interactions(app: &App, limit: usize) -> String {
    let mut lines = vec![
        format!("Recent interactions (last {limit})"),
        "─".repeat(50),
    ];
    match app.gateway.get_interactions(limit).await {
        Ok(rows) => {
            if rows.is_empty() {
                lines.push("No interactions.".to_string());
            } else {
                for r in rows {
                    let outcome = r.outcome.as_deref().unwrap_or("-");
                    let ts =
                        r.ts.as_deref()
                            .or(r.created_at.as_deref())
                            .map(fmt_ts)
                            .unwrap_or_else(|| "-".to_string());
                    lines.push(format!(
                        "[{outcome}] [{channel}] {text}",
                        outcome = outcome,
                        channel = r.channel,
                        text = truncate(&r.request_text, 70)
                    ));
                    let mut meta = format!("  {ts}");
                    if let Some(lat) = r.latency_ms {
                        meta.push_str(&format!(" · {lat} ms"));
                    }
                    if let Some(engine) = &r.engine {
                        meta.push_str(&format!(" · {engine}"));
                    }
                    lines.push(meta);
                }
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_settings(app: &App) -> String {
    let mut lines = vec!["Runtime settings".to_string(), "─".repeat(40)];
    match app.gateway.get_settings_runtime().await {
        Ok(s) => {
            if let Some(runtime) = s.runtime_config {
                lines.push(format!(
                    "Config loaded: {}",
                    if runtime.loaded { "yes" } else { "no" }
                ));
                if let Some(path) = runtime.config_path {
                    lines.push(format!("Config path: {path}"));
                }
                if let Some(name) = runtime.fleet_name {
                    lines.push(format!("Fleet name: {name}"));
                }
                if let Some(port) = runtime.api_port {
                    lines.push(format!("API port: {port}"));
                }
                lines.push(format!(
                    "Nodes configured: {}  Models configured: {}",
                    runtime.nodes_configured.unwrap_or(0),
                    runtime.models_configured.unwrap_or(0)
                ));
            }
            if let Some(db) = s.database {
                lines.push(format!(
                    "Database mode: {}",
                    db.active_mode.as_deref().unwrap_or("-")
                ));
                lines.push(format!(
                    "Database status: {}",
                    db.status.as_deref().unwrap_or("-")
                ));
                if let Some(e) = db.error {
                    lines.push(format!("Database error: {e}"));
                }
            }
            if let Some(tg) = s.telegram {
                lines.push(format!(
                    "Telegram: configured={} enabled={}",
                    tg.configured, tg.enabled
                ));
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_config(app: &App) -> String {
    match app.gateway.get_config_text().await {
        Ok(text) => {
            let mut lines = vec!["fleet.toml".to_string(), "─".repeat(40)];
            for line in text.lines().take(80) {
                lines.push(line.to_string());
            }
            if text.lines().count() > 80 {
                lines.push("...".to_string());
            }
            lines.join("\n")
        }
        Err(e) => format!("Error loading config: {e}"),
    }
}

async fn render_skills(app: &App) -> String {
    let mut lines = vec!["Skills".to_string(), "─".repeat(40)];
    match app.gateway.get_skills().await {
        Ok(skills) => {
            if skills.is_empty() {
                lines.push("No skills found.".to_string());
            } else {
                for s in skills.iter().take(30) {
                    let tools = if s.tools.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", s.tools.join(", "))
                    };
                    lines.push(format!(
                        "{name} ({scope}){tools}",
                        name = s.name,
                        scope = s.scope
                    ));
                    if !s.description.is_empty() {
                        lines.push(format!("  {}", truncate(&s.description, 70)));
                    }
                }
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_brain(app: &App) -> String {
    let mut lines = vec!["Brain threads".to_string(), "─".repeat(40)];
    match app.gateway.get_brain_threads().await {
        Ok(threads) => {
            if threads.is_empty() {
                lines.push("No threads found.".to_string());
            } else {
                for t in threads.iter().take(20) {
                    let status = t.status.as_deref().unwrap_or("-");
                    let ts = t
                        .last_message_at
                        .as_deref()
                        .map(fmt_ts)
                        .unwrap_or_else(|| "-".to_string());
                    let count = t.message_count.unwrap_or(0);
                    lines.push(format!(
                        "[{status}] {title}",
                        status = status,
                        title = t.title
                    ));
                    lines.push(format!(
                        "  slug: {} · messages: {} · last: {}",
                        t.slug, count, ts
                    ));
                }
            }
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}

async fn render_status(app: &App) -> String {
    let mut lines = vec!["Gateway status".to_string(), "─".repeat(40)];
    match app.gateway.get_fleet_status().await {
        Ok(s) => {
            lines.push(format!(
                "Gateway status: {}",
                s.status.as_deref().unwrap_or("unknown")
            ));
            if let Some(scanned) = s.scanned_at {
                lines.push(format!("Scanned at: {}", fmt_ts(&scanned)));
            }
            let summary = s.summary.unwrap_or_default();
            lines.push(format!(
                "Nodes: {} online / {} total",
                summary.connected_nodes.unwrap_or(0),
                summary.total_nodes.unwrap_or(0)
            ));
        }
        Err(e) => lines.push(format!("Error: {e}")),
    }
    lines.join("\n")
}
