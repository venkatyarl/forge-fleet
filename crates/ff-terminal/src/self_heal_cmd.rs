//! `ff self-heal {status,pause,freeze-tier,revert,trust}` — operator
//! escape-hatches for the self-heal coordination loop. Full tick + PR
//! pipeline lands in a later phase (see self-heal-coordination.md).

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
            let tr: i32 = r.try_get("attempts")?;
            println!(
                "{:<18} {:<4} {:<14} {:>6} {:>5}",
                sig.chars().take(18).collect::<String>(),
                tier,
                status.chars().take(14).collect::<String>(),
                cnt,
                tr
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

pub async fn handle_revert(_pg: &PgPool, bug_sig: &str) -> Result<()> {
    println!("TODO: rollback fix for bug_signature={}", bug_sig);
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
