use crate::whoami_tag;
use anyhow::Result;

/// Documented `deferred_tasks.status` values (see schema.rs `deferred_tasks`).
/// Used to WARN on a likely-typo'd `--status` filter. Deliberately a soft
/// warning, not a hard error — same rationale as `tasks_cmd::KNOWN_TASK_STATUSES`:
/// a future status added to the schema should only cost a spurious warning,
/// never a wrongful rejection.
const KNOWN_DEFER_STATUSES: &[&str] = &[
    "pending",
    "dispatchable",
    "running",
    "completed",
    "failed",
    "cancelled",
];

/// Returns the comma-separated `--status` parts that aren't a documented
/// deferred-task status (likely typos). Pure — empty filter / all-known →
/// empty vec. Match is case-insensitive so `Running` doesn't warn.
fn unknown_defer_status_parts(filter: &str) -> Vec<String> {
    filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !KNOWN_DEFER_STATUSES.contains(&s.to_ascii_lowercase().as_str()))
        .map(|s| s.to_string())
        .collect()
}

/// Terminal `deferred_tasks.status` values — a `--watch` poll stops here. A
/// deferred task starts pending/dispatchable, runs, and ends completed/failed/
/// cancelled. Mirrors `tasks_cmd::is_terminal_task_status`.
fn is_terminal_defer_status(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "completed" | "failed" | "cancelled"
    )
}

/// Poll a deferred task's status until it reaches a terminal state, printing a
/// progress line to stderr only when something changes (status / attempts /
/// last_error) so a long watch doesn't spam identical lines. Bounded by a hard
/// cap so a never-fired trigger can't hang the CLI forever. Mirrors
/// `ff tasks get --watch` (3s poll, 3600s cap, dedup'd stderr).
async fn watch_deferred_until_terminal(pool: &sqlx::PgPool, id: &str) -> Result<()> {
    use crate::{CYAN, RESET};
    const POLL_SECS: u64 = 3;
    const MAX_WAIT_SECS: u64 = 3600;
    let mut waited = 0u64;
    let mut last_line = String::new();
    loop {
        let Some(r) = ff_db::pg_get_deferred(pool, id).await? else {
            anyhow::bail!("No deferred task with id '{id}'");
        };
        if is_terminal_defer_status(&r.status) {
            break;
        }
        let line = format!(
            "● {}  (attempt {}/{}){}",
            r.status,
            r.attempts,
            r.max_attempts,
            r.last_error
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|e| format!("  — {e}"))
                .unwrap_or_default(),
        );
        if line != last_line {
            eprintln!("{CYAN}{line}{RESET}  \x1b[2m(waited {waited}s){RESET}");
            last_line = line;
        }
        if waited >= MAX_WAIT_SECS {
            eprintln!(
                "\x1b[2m  watch timeout after {waited}s — task still {}; \
                 re-run --watch to keep polling{RESET}",
                r.status
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(POLL_SECS)).await;
        waited += POLL_SECS;
    }
    Ok(())
}

/// Human-readable trigger label for the table's TRIGGER column. Pure — derives
/// from `trigger_type` + `trigger_spec` exactly as the text path did, extracted
/// so the JSON row and the table stay in lockstep.
fn trigger_label(trigger_type: &str, trigger_spec: &serde_json::Value) -> String {
    match trigger_type {
        "node_online" => trigger_spec
            .get("node")
            .and_then(|v| v.as_str())
            .map(|n| format!("node={n}"))
            .unwrap_or_else(|| "node_online".into()),
        "at_time" => trigger_spec
            .get("at")
            .and_then(|v| v.as_str())
            .unwrap_or("at_time")
            .to_string(),
        other => other.to_string(),
    }
}

/// Lossless JSON projection of one deferred-task row. Full untruncated `title`
/// (the table elides nothing but is fixed-width), the raw `trigger_type` +
/// `trigger_spec` AND the derived `trigger` label, RFC3339 timestamps, and the
/// nullable claim/error fields. Pure (no DB/clock). The full `payload`/`result`
/// stay on `ff defer get` (mirrors tasks list omitting result — #250).
fn defer_list_json_row(r: &ff_db::DeferredTaskRow) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "title": r.title,
        "kind": r.kind,
        "status": r.status,
        "trigger_type": r.trigger_type,
        "trigger_spec": r.trigger_spec,
        "trigger": trigger_label(&r.trigger_type, &r.trigger_spec),
        "preferred_node": r.preferred_node,
        "attempts": r.attempts,
        "max_attempts": r.max_attempts,
        "created_by": r.created_by,
        "claimed_by": r.claimed_by,
        "last_error": r.last_error,
        "created_at": r.created_at.to_rfc3339(),
        "next_attempt_at": r.next_attempt_at.map(|t| t.to_rfc3339()),
        "claimed_at": r.claimed_at.map(|t| t.to_rfc3339()),
        "completed_at": r.completed_at.map(|t| t.to_rfc3339()),
    })
}

pub async fn handle_defer(cmd: crate::DeferCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        crate::DeferCommand::List {
            status,
            limit,
            json,
        } => {
            // Warn (don't reject — see KNOWN_DEFER_STATUSES) on a likely typo so an
            // empty result isn't mistaken for "no matching tasks". Runs BEFORE the
            // json branch so a typo'd filter is surfaced in both modes (consistent
            // with tasks_cmd / #250).
            if let Some(sf) = status.as_deref() {
                for bad in unknown_defer_status_parts(sf) {
                    eprintln!(
                        "warning: unknown status '{bad}' — known: {}",
                        KNOWN_DEFER_STATUSES.join(", ")
                    );
                }
            }
            let rows = ff_db::pg_list_deferred(&pool, status.as_deref(), limit).await?;
            if json {
                let arr: Vec<serde_json::Value> = rows.iter().map(defer_list_json_row).collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("(no deferred tasks)");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<12} {:<16} {:<6} TITLE",
                "ID", "STATUS", "TRIGGER", "TARGET", "TRY"
            );
            for r in rows {
                let trigger = trigger_label(&r.trigger_type, &r.trigger_spec);
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
        crate::DeferCommand::Get { id, watch } => {
            // --watch: block until the task is terminal (streaming status
            // changes to stderr), then fall through to the one-shot detail print.
            if watch {
                watch_deferred_until_terminal(&pool, &id).await?;
            }
            match ff_db::pg_get_deferred(&pool, &id).await? {
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
                                serde_json::to_string_pretty(res)
                                    .unwrap_or_else(|_| res.to_string())
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
            }
        }
        crate::DeferCommand::Cancel { id, force } => {
            let cancelled = if force {
                ff_db::pg_force_cancel_deferred(&pool, &id).await?
            } else {
                ff_db::pg_cancel_deferred(&pool, &id).await?
            };
            if cancelled {
                println!("Cancelled task {id}");
            } else if force {
                // The requested mutation did NOT happen (task already terminal
                // or does not exist). Report on stderr + exit non-zero so a
                // script's `&&` chain / `$?` check sees the failure instead of a
                // false success — mirrors `ff defer get` on a missing id.
                eprintln!("Task {id} not cancellable (already terminal, or does not exist)");
                std::process::exit(1);
            } else {
                eprintln!(
                    "Task {id} is not in a cancellable state (use --force for a stuck 'running' task)"
                );
                std::process::exit(1);
            }
        }
        crate::DeferCommand::Retry { id } => {
            if ff_db::pg_retry_deferred(&pool, &id).await? {
                println!("Task {id} requeued for retry (status=pending)");
            } else {
                // No row updated — the task is missing or not in a
                // failed/cancelled state, so the retry didn't happen. Signal it
                // with a non-zero exit (consistent with `ff defer get` and the
                // cortex/model no-match verbs) instead of a silent success.
                eprintln!("Task {id} is not in a retryable state (must be failed or cancelled)");
                std::process::exit(1);
            }
        }
        crate::DeferCommand::Stats { window_hours, json } => {
            let stats = ff_db::pg_deferred_stats(&pool, window_hours).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                print!("{}", render_deferred_stats(&stats));
            }
        }
    }
    Ok(())
}

/// Render a [`ff_db::DeferredStats`] as a human report (pure; unit-tested).
fn render_deferred_stats(s: &ff_db::DeferredStats) -> String {
    let mut out = String::new();
    let total: i64 = s.by_status.iter().map(|c| c.count).sum();
    out.push_str(&format!("deferred_tasks queue — {total} rows total\n"));

    out.push_str("\n  by status:\n");
    for c in &s.by_status {
        out.push_str(&format!("    {:<12} {}\n", c.label, c.count));
    }

    out.push_str(&format!(
        "\n  created in last {}h (flood detector):\n",
        s.window_hours
    ));
    if s.recent_created.is_empty() {
        out.push_str("    (none)\n");
    }
    for c in &s.recent_created {
        out.push_str(&format!("    {:>5}  {}\n", c.count, c.label));
    }

    out.push_str(&format!("\n  failures in last {}h:\n", s.window_hours));
    if s.recent_failures.is_empty() {
        out.push_str("    (none) ✓\n");
    }
    for c in &s.recent_failures {
        out.push_str(&format!("    {:>5}  {}\n", c.count, c.label));
    }

    match (&s.oldest_pending_id, s.oldest_pending_age_secs) {
        (Some(id), Some(age)) => out.push_str(&format!(
            "\n  oldest pending: {id} ({})\n",
            humanize_secs(age)
        )),
        _ => out.push_str("\n  oldest pending: (none)\n"),
    }
    out
}

/// Compact human duration for an age in seconds (e.g. `2h13m`, `4d1h`, `45s`).
fn humanize_secs(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86_400, (s % 86_400) / 3600, (s % 3600) / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_defer_status_matches_only_terminal_states() {
        // Terminal — a --watch poll stops here.
        for s in ["completed", "failed", "cancelled", "COMPLETED", "Failed"] {
            assert!(is_terminal_defer_status(s), "{s} should be terminal");
        }
        // Non-terminal — the task is still progressing; watch keeps polling.
        for s in ["pending", "dispatchable", "running", "Pending"] {
            assert!(!is_terminal_defer_status(s), "{s} should NOT be terminal");
        }
    }

    #[test]
    fn humanize_secs_picks_the_right_unit() {
        assert_eq!(humanize_secs(45), "45s");
        assert_eq!(humanize_secs(130), "2m");
        assert_eq!(humanize_secs(3 * 3600 + 13 * 60), "3h13m");
        assert_eq!(humanize_secs(4 * 86_400 + 3600), "4d1h");
        assert_eq!(humanize_secs(-5), "0s"); // clamped, never panics
    }

    #[test]
    fn render_deferred_stats_shows_sections_and_totals() {
        let s = ff_db::DeferredStats {
            window_hours: 3,
            by_status: vec![
                ff_db::DeferredCount {
                    label: "completed".into(),
                    count: 100,
                },
                ff_db::DeferredCount {
                    label: "failed".into(),
                    count: 5,
                },
            ],
            recent_failures: vec![ff_db::DeferredCount {
                label: "exit 1: boom".into(),
                count: 2,
            }],
            recent_created: vec![ff_db::DeferredCount {
                label: "rsync postgres".into(),
                count: 56,
            }],
            oldest_pending_id: Some("abc-123".into()),
            oldest_pending_age_secs: Some(7200),
        };
        let out = render_deferred_stats(&s);
        assert!(out.contains("105 rows total")); // 100 + 5
        assert!(out.contains("completed    100"));
        assert!(out.contains("flood detector"));
        assert!(out.contains("rsync postgres"));
        assert!(out.contains("exit 1: boom"));
        assert!(out.contains("oldest pending: abc-123 (2h0m)"));
    }

    #[test]
    fn render_deferred_stats_handles_clean_and_empty() {
        let s = ff_db::DeferredStats {
            window_hours: 6,
            by_status: vec![],
            recent_failures: vec![],
            recent_created: vec![],
            oldest_pending_id: None,
            oldest_pending_age_secs: None,
        };
        let out = render_deferred_stats(&s);
        assert!(out.contains("0 rows total"));
        assert!(out.contains("(none) ✓")); // no failures
        assert!(out.contains("oldest pending: (none)"));
    }

    #[test]
    fn trigger_label_derives_from_spec() {
        assert_eq!(
            trigger_label("node_online", &serde_json::json!({"node": "ace"})),
            "node=ace"
        );
        // node_online with no node key falls back to the bare type.
        assert_eq!(
            trigger_label("node_online", &serde_json::json!({})),
            "node_online"
        );
        assert_eq!(
            trigger_label(
                "at_time",
                &serde_json::json!({"at": "2026-06-13T00:00:00Z"})
            ),
            "2026-06-13T00:00:00Z"
        );
        // Unknown trigger types pass through verbatim.
        assert_eq!(trigger_label("manual", &serde_json::json!({})), "manual");
    }

    #[test]
    fn defer_list_json_row_is_lossless() {
        let created = chrono::DateTime::parse_from_rfc3339("2026-06-13T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // A pending node_online task: claim/error/next-attempt all null.
        let row = ff_db::DeferredTaskRow {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            created_at: created,
            created_by: Some("operator".to_string()),
            title: "Ollama cleanup on ace".to_string(),
            kind: "shell".to_string(),
            payload: serde_json::json!({"run": "rm -rf ~/.ollama"}),
            trigger_type: "node_online".to_string(),
            trigger_spec: serde_json::json!({"node": "ace"}),
            preferred_node: Some("ace".to_string()),
            required_caps: serde_json::json!([]),
            status: "pending".to_string(),
            attempts: 0,
            max_attempts: 5,
            next_attempt_at: None,
            claimed_by: None,
            claimed_at: None,
            last_error: None,
            result: None,
            completed_at: None,
        };
        let v = defer_list_json_row(&row);
        assert_eq!(v["id"], "11111111-1111-1111-1111-111111111111");
        assert_eq!(v["title"], "Ollama cleanup on ace");
        assert_eq!(v["status"], "pending");
        assert_eq!(v["trigger_type"], "node_online");
        assert_eq!(v["trigger"], "node=ace"); // derived label preserved
        assert_eq!(v["trigger_spec"]["node"], "ace"); // raw spec preserved
        assert_eq!(v["preferred_node"], "ace");
        assert_eq!(v["attempts"], 0);
        assert_eq!(v["max_attempts"], 5);
        assert_eq!(v["created_at"], "2026-06-13T10:00:00+00:00");
        // Nullable fields are JSON null, not omitted, so the shape is stable.
        assert!(v["next_attempt_at"].is_null());
        assert!(v["claimed_by"].is_null());
        assert!(v["last_error"].is_null());
        // Full payload is intentionally NOT in the list projection.
        assert!(v.get("payload").is_none());
    }

    #[test]
    fn unknown_defer_status_parts_flags_typos_only() {
        // Empty / whitespace → nothing to warn about.
        assert!(unknown_defer_status_parts("").is_empty());
        assert!(unknown_defer_status_parts("  ").is_empty());
        // All-known (with surrounding whitespace) → empty.
        assert!(unknown_defer_status_parts("pending, running ,completed").is_empty());
        // Case-insensitive: a capitalized known status doesn't warn.
        assert!(unknown_defer_status_parts("Running,FAILED").is_empty());
        // A typo is surfaced, valid siblings are not.
        assert_eq!(
            unknown_defer_status_parts("running,runing,pending"),
            vec!["runing".to_string()]
        );
        // Every documented status is recognized (guards against drift between
        // KNOWN_DEFER_STATUSES and the schema comment at deferred_tasks.status).
        for s in KNOWN_DEFER_STATUSES {
            assert!(
                unknown_defer_status_parts(s).is_empty(),
                "status {s} should be known"
            );
        }
    }
}
