//! `ff self-heal {status,pause,freeze-tier,revert,trust}` — operator
//! escape-hatches for the self-heal coordination loop. Implementation is
//! stubbed; the tick + PR pipeline lands in a later phase.

use anyhow::Result;
use sqlx::{PgPool, Row};

pub async fn handle_status(pg: &PgPool) -> Result<()> {
    println!("── Self-heal queue ─────────────────────────────────────");
    let rows = sqlx::query(
        "SELECT bug_signature, tier, status, report_count, attempts, created_at \
         FROM fleet_self_heal_queue ORDER BY created_at DESC LIMIT 20",
    )
    .fetch_all(pg)
    .await?;
    if rows.is_empty() {
        println!("  (no entries)");
    } else {
        println!(
            "{:<18} {:<4} {:<14} {:>6} {:>5}",
            "SIG", "TIER", "STATUS", "CNT", "TRY"
        );
        for r in rows {
            let sig: String = r.try_get("bug_signature")?;
            let tier: String = r.try_get("tier")?;
            let status: String = r.try_get("status")?;
            let cnt: i32 = r.try_get("report_count")?;
            let try_: i32 = r.try_get("attempts")?;
            println!(
                "{:<18} {:<4} {:<14} {:>6} {:>5}",
                sig.chars().take(18).collect::<String>(),
                tier,
                status.chars().take(14).collect::<String>(),
                cnt,
                try_
            );
        }
    }
    println!();
    println!("── Daemon trust scores ─────────────────────────────────");
    let trust_rows = sqlx::query(
        "SELECT c.name, t.tier, t.current_level, t.clean_fixes, t.reverted_fixes \
         FROM daemon_trust_scores t JOIN computers c ON t.computer_id = c.id \
         ORDER BY c.name, t.tier",
    )
    .fetch_all(pg)
    .await?;
    if trust_rows.is_empty() {
        println!("  (no trust records yet)");
    } else {
        println!(
            "{:<12} {:<4} {:<20} {:>5} {:>5}",
            "COMPUTER", "TIER", "LEVEL", "CLEAN", "REV"
        );
        for r in trust_rows {
            let name: String = r.try_get("name")?;
            let tier: String = r.try_get("tier")?;
            let lvl: String = r.try_get("current_level")?;
            let clean: i32 = r.try_get("clean_fixes")?;
            let rev: i32 = r.try_get("reverted_fixes")?;
            println!(
                "{:<12} {:<4} {:<20} {:>5} {:>5}",
                name.chars().take(12).collect::<String>(),
                tier,
                lvl.chars().take(20).collect::<String>(),
                clean,
                rev
            );
        }
    }
    Ok(())
}

pub async fn handle_pause(pg: &PgPool) -> Result<()> {
    sqlx::query(
        "UPDATE fleet_self_heal_queue SET status = 'paused' \
         WHERE status IN ('detected', 'fixing', 'reviewing')",
    )
    .execute(pg)
    .await?;
    println!("Paused all in-flight self-heal fixes.");
    Ok(())
}

pub async fn handle_freeze_tier(pg: &PgPool, tier: &str, hours: u32) -> Result<()> {
    let until = chrono::Utc::now() + chrono::Duration::hours(hours as i64);
    sqlx::query("UPDATE daemon_trust_scores SET probation_until = $1 WHERE tier = $2")
        .bind(until)
        .bind(tier)
        .execute(pg)
        .await?;
    println!(
        "Tier {} frozen until {} (human approval required)",
        tier, until
    );
    Ok(())
}

pub async fn handle_revert(pg: &PgPool, bug_sig: &str) -> Result<()> {
    let row = sqlx::query(
        "SELECT id, bug_signature, tier, status, rollback_playbook, computer_id \
         FROM fleet_self_heal_queue WHERE bug_signature = $1",
    )
    .bind(bug_sig)
    .fetch_optional(pg)
    .await?;

    let Some(row) = row else {
        println!("No self-heal entry found for bug_signature={}", bug_sig);
        return Ok(());
    };

    let id: i64 = row.try_get("id")?;
    let status: String = row.try_get("status")?;
    let rollback: serde_json::Value = row.try_get("rollback_playbook")?;

    if status == "reverted" {
        println!("Fix {} is already reverted.", bug_sig);
        return Ok(());
    }

    // If a rollback playbook exists, enqueue it as a deferred task.
    if let Some(cmd) = rollback.as_object().and_then(|o| o.get("command")).and_then(|c| c.as_str()) {
        let task_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO deferred_tasks (id, kind, payload, status, priority, created_at, meta) \
             VALUES ($1, 'shell', $2, 'queued', 100, NOW(), $3)",
        )
        .bind(task_id)
        .bind(serde_json::json!({ "command": cmd }))
        .bind(serde_json::json!({
            "self_heal_revert": { "queue_id": id, "bug_signature": bug_sig }
        }))
        .execute(pg)
        .await?;
        println!("Enqueued rollback task {} for bug_signature={}", task_id, bug_sig);
    } else {
        println!(
            "No rollback playbook for bug_signature={}; marking reverted without action.",
            bug_sig
        );
    }

    sqlx::query(
        "UPDATE fleet_self_heal_queue \
         SET status = 'reverted', updated_at = NOW() \
         WHERE id = $1",
    )
    .bind(id)
    .execute(pg)
    .await?;

    println!("Marked bug_signature={} as reverted.", bug_sig);
    Ok(())
}

pub async fn handle_trust_reset(pg: &PgPool, computer: &str) -> Result<()> {
    sqlx::query(
        "UPDATE daemon_trust_scores SET current_level = 'operator_approve', clean_fixes = 0 \
         WHERE computer_id = (SELECT id FROM computers WHERE name = $1)",
    )
    .bind(computer)
    .execute(pg)
    .await?;
    println!(
        "Trust reset for {}: back to operator-approve probation",
        computer
    );
    Ok(())
}

/// Internal pipeline entry: the leader enqueues a deferred task that runs
/// `ff self-heal run-writer --bug-sig <sig>`. This stub records the attempt
/// and prints the bug context; the actual LLM writer logic will be wired in
/// once the `--role writer` supervise path is built.
pub async fn handle_run_writer(pg: &PgPool, bug_sig: &str) -> Result<()> {
    let row = sqlx::query(
        "SELECT bug_signature, tier, status, report_count, attempts, created_at \
         FROM fleet_self_heal_queue WHERE bug_signature = $1",
    )
    .bind(bug_sig)
    .fetch_optional(pg)
    .await?;

    let Some(row) = row else {
        println!("No self-heal queue entry found for bug_signature={}", bug_sig);
        return Ok(());
    };

    let sig: String = row.try_get("bug_signature")?;
    let tier: String = row.try_get("tier")?;
    let status: String = row.try_get("status")?;
    let report_count: i32 = row.try_get("report_count")?;
    let attempts: i32 = row.try_get("attempts")?;

    // Pull the most recent bug report for context.
    let detail = sqlx::query(
        "SELECT file_path, line_number, error_class, stack_excerpt, binary_version \
         FROM fleet_bug_reports WHERE bug_signature = $1 ORDER BY reported_at DESC LIMIT 1",
    )
    .bind(bug_sig)
    .fetch_optional(pg)
    .await?;

    println!("── Self-heal writer run ────────────────────────────────");
    println!("bug_signature : {}", sig);
    println!("tier          : {}", tier);
    println!("status        : {}", status);
    println!("report_count  : {}", report_count);
    println!("attempts      : {}", attempts);

    if let Some(d) = detail {
        let file: Option<String> = d.try_get("file_path").ok();
        let line: Option<i32> = d.try_get("line_number").ok();
        let class: String = d.try_get("error_class")?;
        let stack: Option<String> = d.try_get("stack_excerpt").ok();
        let version: Option<String> = d.try_get("binary_version").ok();
        println!(
            "location      : {}:{}",
            file.as_deref().unwrap_or("?"),
            line.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
        );
        println!("error_class   : {}", class);
        if let Some(v) = version {
            println!("binary_version: {}", v);
        }
        if let Some(s) = stack {
            println!("stack_excerpt : {}", s);
        }
    }

    println!();
    println!("[STUB] Actual LLM writer + fix generation not yet implemented.");
    println!("       The pipeline is end-to-end: bug → beat → queue → this stub.");
    println!("       Wire `ff supervise --role writer` here when ready.");

    // Record that the writer ran so the leader knows not to re-dispatch
    // immediately. The leader sets status='fixing' on enqueue; we keep it
    // as 'fixing' until a real writer produces a branch/PR.
    sqlx::query(
        "UPDATE fleet_self_heal_queue \
         SET last_attempt_at = NOW() \
         WHERE bug_signature = $1",
    )
    .bind(bug_sig)
    .execute(pg)
    .await?;

    Ok(())
}
