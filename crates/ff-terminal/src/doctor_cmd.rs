//! `ff doctor` — one aggregate fleet self-check.
//!
//! Composes the health signals an operator otherwise gathers one command at a
//! time (`ff defer stats`, `ff interactions stats`, `ff pm doctor`,
//! `ff alert doctor`) into a single PASS/WARN/FAIL verdict + per-check rows.
//! Every check is cheap (one query) and read-only.

use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;

/// A check's health. Ordered so `max` gives the worst (overall) verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Health {
    Pass,
    Warn,
    Fail,
}

impl Health {
    fn glyph(self) -> &'static str {
        match self {
            Health::Pass => "✓",
            Health::Warn => "⚠",
            Health::Fail => "✗",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Health::Pass => "PASS",
            Health::Warn => "WARN",
            Health::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct DoctorCheck {
    name: String,
    status: Health,
    detail: String,
}

/// Worst-of: FAIL if any check failed, else WARN if any warned, else PASS. Pure.
fn overall_health(checks: &[DoctorCheck]) -> Health {
    checks
        .iter()
        .map(|c| c.status)
        .max()
        .unwrap_or(Health::Pass)
}

// ── Per-check classifiers (pure, so the thresholds are unit-tested) ──────────

/// Deferred-task failures in the recent window. A handful is noise; a flood is
/// the symptom class the loop chased in #560–#566.
fn defer_health(failures: i64) -> Health {
    match failures {
        0 => Health::Pass,
        1..=20 => Health::Warn,
        _ => Health::Fail,
    }
}

/// ff_interactions token-logging gap = recent rows missing tokens. Only mean-
/// ingful with enough volume, so a tiny sample never trips it.
fn token_gap_health(recent: i64, zero_token: i64) -> Health {
    if recent < 5 {
        return Health::Pass;
    }
    let pct = (zero_token as f64 / recent as f64) * 100.0;
    if pct >= 50.0 {
        Health::Warn
    } else {
        Health::Pass
    }
}

/// Orphaned `in_progress` work_items (no active lease) — the scheduler sweeps
/// them hourly, so a non-zero count is a transient warning, not a failure.
fn orphan_health(n: i64) -> Health {
    if n > 0 { Health::Warn } else { Health::Pass }
}

/// Enabled alert policies that cannot fire (the dead-policy class, #583/#584).
fn dead_alert_health(n: i64) -> Health {
    if n > 0 { Health::Warn } else { Health::Pass }
}

/// Leader liveness: a stale/absent leader heartbeat fails — leader-gated ticks
/// (autoscaler, reapers, upgrades) stop running without a fresh leader.
fn leader_health(fresh: bool) -> Health {
    if fresh { Health::Pass } else { Health::Fail }
}

/// Render the report. Pure (no I/O / color in the assertions matter) so the
/// layout is unit-testable.
fn render_doctor(checks: &[DoctorCheck], overall: Health) -> String {
    let mut out = String::new();
    out.push_str(&format!("{CYAN}▶ ff doctor — fleet self-check{RESET}\n\n"));
    for c in checks {
        let color = match c.status {
            Health::Pass => GREEN,
            Health::Warn => YELLOW,
            Health::Fail => RED,
        };
        out.push_str(&format!(
            "  {color}{} {:<5}{RESET} {:<22} {}\n",
            c.status.glyph(),
            c.status.label(),
            c.name,
            c.detail
        ));
    }
    let color = match overall {
        Health::Pass => GREEN,
        Health::Warn => YELLOW,
        Health::Fail => RED,
    };
    out.push_str(&format!(
        "\n{color}{} Overall: {}{RESET}\n",
        overall.glyph(),
        overall.label()
    ));
    out
}

/// `ff doctor [--json]`.
pub async fn handle_doctor(json: bool) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let mut checks: Vec<DoctorCheck> = Vec::new();

    // 1) Deferred-task failures (last 3h).
    let defer = ff_db::queries::pg_deferred_stats(&pool, 3).await?;
    let defer_failures: i64 = defer.recent_failures.iter().map(|c| c.count).sum();
    checks.push(DoctorCheck {
        name: "deferred failures".into(),
        status: defer_health(defer_failures),
        detail: format!("{defer_failures} in last 3h"),
    });

    // 2) ff_interactions token-logging gap (last 24h).
    let inter = ff_db::queries::pg_interaction_stats(&pool, 24).await?;
    checks.push(DoctorCheck {
        name: "interaction tokens".into(),
        status: token_gap_health(inter.recent, inter.recent_zero_token),
        detail: format!(
            "{}/{} recent rows missing tokens",
            inter.recent_zero_token, inter.recent
        ),
    });

    // 3) Orphaned work_items (in_progress with no active lease, >1h).
    let orphans = ff_db::pg_count_orphaned_work_items(&pool, 3600).await?;
    checks.push(DoctorCheck {
        name: "work_item orphans".into(),
        status: orphan_health(orphans),
        detail: format!("{orphans} orphaned in_progress"),
    });

    // 4) Alert policies that can't fire (enabled but dead).
    let policies: Vec<(String, String, bool)> =
        sqlx::query_as("SELECT metric, condition, enabled FROM alert_policies")
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("read alert_policies: {e}"))?;
    let dead_alerts = policies
        .iter()
        .filter(|(metric, condition, enabled)| {
            *enabled
                && !ff_agent::alert_evaluator::classify_policy_fireability(metric, condition)
                    .can_fire()
        })
        .count() as i64;
    checks.push(DoctorCheck {
        name: "alert policies".into(),
        status: dead_alert_health(dead_alerts),
        detail: format!("{dead_alerts} enabled but cannot fire"),
    });

    // 5) Leader liveness (fresh heartbeat within 60s).
    let fresh_leaders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_leader_state WHERE heartbeat_at > NOW() - INTERVAL '60 seconds'",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("leader check: {e}"))?;
    checks.push(DoctorCheck {
        name: "leader liveness".into(),
        status: leader_health(fresh_leaders > 0),
        detail: if fresh_leaders > 0 {
            "fresh heartbeat".into()
        } else {
            "no leader heartbeat in 60s".into()
        },
    });

    let overall = overall_health(&checks);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "overall": overall, "checks": checks })
            )?
        );
    } else {
        print!("{}", render_doctor(&checks, overall));
    }

    // Non-zero exit on FAIL so the loop / scripts can gate on it.
    if overall == Health::Fail {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_is_worst_of() {
        let mk = |s: Health| DoctorCheck {
            name: "x".into(),
            status: s,
            detail: String::new(),
        };
        assert_eq!(overall_health(&[]), Health::Pass);
        assert_eq!(
            overall_health(&[mk(Health::Pass), mk(Health::Pass)]),
            Health::Pass
        );
        assert_eq!(
            overall_health(&[mk(Health::Pass), mk(Health::Warn)]),
            Health::Warn
        );
        assert_eq!(
            overall_health(&[mk(Health::Warn), mk(Health::Fail)]),
            Health::Fail
        );
    }

    #[test]
    fn defer_thresholds() {
        assert_eq!(defer_health(0), Health::Pass);
        assert_eq!(defer_health(5), Health::Warn);
        assert_eq!(defer_health(21), Health::Fail);
    }

    #[test]
    fn token_gap_needs_volume_then_flags() {
        // Tiny sample never trips, even at 100% missing.
        assert_eq!(token_gap_health(2, 2), Health::Pass);
        // Enough volume + majority missing → Warn.
        assert_eq!(token_gap_health(10, 6), Health::Warn);
        // Enough volume but mostly logged → Pass.
        assert_eq!(token_gap_health(10, 1), Health::Pass);
    }

    #[test]
    fn orphan_dead_alert_leader() {
        assert_eq!(orphan_health(0), Health::Pass);
        assert_eq!(orphan_health(3), Health::Warn);
        assert_eq!(dead_alert_health(0), Health::Pass);
        assert_eq!(dead_alert_health(1), Health::Warn);
        assert_eq!(leader_health(true), Health::Pass);
        assert_eq!(leader_health(false), Health::Fail);
    }

    #[test]
    fn render_includes_rows_and_overall() {
        let checks = vec![
            DoctorCheck {
                name: "deferred failures".into(),
                status: Health::Pass,
                detail: "0 in last 3h".into(),
            },
            DoctorCheck {
                name: "leader liveness".into(),
                status: Health::Fail,
                detail: "no leader heartbeat in 60s".into(),
            },
        ];
        let out = render_doctor(&checks, overall_health(&checks));
        assert!(out.contains("ff doctor"));
        assert!(out.contains("deferred failures"));
        assert!(out.contains("leader liveness"));
        assert!(out.contains("Overall: FAIL"));
    }
}
