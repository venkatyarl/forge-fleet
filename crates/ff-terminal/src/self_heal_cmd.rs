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
