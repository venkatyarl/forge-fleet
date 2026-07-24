//! `ff doctor` — one aggregate fleet self-check.
//!
//! Composes the health signals an operator otherwise gathers one command at a
//! time (`ff defer stats`, `ff interactions stats`, `ff pm doctor`,
//! `ff alert doctor`) into a single PASS/WARN/FAIL verdict + per-check rows.
//! Every check is cheap (one query) and read-only.

use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, serde::Serialize)]
struct OpenErrorSignature {
    signature: String,
    state: String,
    count_24h: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum ErrorKpiVerdict {
    Improving,
    Flat,
    Regressing,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ErrorReport {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    signatures_by_state: BTreeMap<String, i64>,
    resolution_rate_30d: Option<f64>,
    resolved_30d: i64,
    resolution_denominator_30d: i64,
    mttr_hours_30d: Option<f64>,
    current_week_mttr_hours: Option<f64>,
    previous_week_mttr_hours: Option<f64>,
    top_open: Vec<OpenErrorSignature>,
    verdict: ErrorKpiVerdict,
}

fn error_kpi_verdict(current: Option<f64>, previous: Option<f64>) -> ErrorKpiVerdict {
    match (current, previous) {
        (Some(current), Some(previous)) if current < previous => ErrorKpiVerdict::Improving,
        (Some(current), Some(previous)) if current > previous => ErrorKpiVerdict::Regressing,
        _ => ErrorKpiVerdict::Flat,
    }
}

fn unavailable_error_report(reason: &str) -> ErrorReport {
    ErrorReport {
        available: false,
        reason: Some(reason.into()),
        signatures_by_state: BTreeMap::new(),
        resolution_rate_30d: None,
        resolved_30d: 0,
        resolution_denominator_30d: 0,
        mttr_hours_30d: None,
        current_week_mttr_hours: None,
        previous_week_mttr_hours: None,
        top_open: Vec::new(),
        verdict: ErrorKpiVerdict::Flat,
    }
}

async fn collect_error_report(pool: &sqlx::PgPool) -> Result<ErrorReport> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.error_signatures') IS NOT NULL")
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("ErrorMiner table check: {e}"))?;
    if !table_exists {
        return Ok(unavailable_error_report(
            "ErrorMiner-1 schema not installed",
        ));
    }

    let lifecycle_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'error_signatures'
            AND column_name IN ('first_seen_at', 'updated_at', 'resolved_at')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("ErrorMiner lifecycle field check: {e}"))?;
    if lifecycle_columns != 3 {
        return Ok(unavailable_error_report(
            "ErrorMiner lifecycle display fields not installed",
        ));
    }

    let state_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, COUNT(*)::BIGINT
           FROM error_signatures
          GROUP BY state
          ORDER BY state",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("ErrorMiner signatures by state: {e}"))?;
    let signatures_by_state = state_rows.into_iter().collect();

    let (resolved_30d, denominator_30d, mttr_hours_30d): (i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT
                 COUNT(*) FILTER (
                     WHERE state = 'resolved'
                       AND updated_at >= NOW() - INTERVAL '30 days')::BIGINT,
                 COUNT(*) FILTER (
                     WHERE state IN ('resolved', 'regressed', 'filed')
                       AND updated_at >= NOW() - INTERVAL '30 days')::BIGINT,
                 EXTRACT(EPOCH FROM AVG(resolved_at - first_seen_at) FILTER (
                     WHERE state = 'resolved'
                       AND resolved_at >= NOW() - INTERVAL '30 days'
                       AND resolved_at >= first_seen_at))::DOUBLE PRECISION / 3600.0
               FROM error_signatures",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("ErrorMiner 30d metrics: {e}"))?;
    let resolution_rate_30d =
        (denominator_30d > 0).then(|| resolved_30d as f64 / denominator_30d as f64);

    let (current_week_mttr_hours, previous_week_mttr_hours): (Option<f64>, Option<f64>) =
        sqlx::query_as(
            "SELECT
                 EXTRACT(EPOCH FROM AVG(resolved_at - first_seen_at) FILTER (
                     WHERE state = 'resolved'
                       AND resolved_at >= NOW() - INTERVAL '7 days'
                       AND resolved_at < NOW()
                       AND resolved_at >= first_seen_at))::DOUBLE PRECISION / 3600.0,
                 EXTRACT(EPOCH FROM AVG(resolved_at - first_seen_at) FILTER (
                     WHERE state = 'resolved'
                       AND resolved_at >= NOW() - INTERVAL '14 days'
                       AND resolved_at < NOW() - INTERVAL '7 days'
                       AND resolved_at >= first_seen_at))::DOUBLE PRECISION / 3600.0
               FROM error_signatures",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("ErrorMiner weekly MTTR: {e}"))?;

    let top_open: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT signature, state, count_24h
           FROM error_signatures
          WHERE state IN ('new', 'filed', 'regressed')
          ORDER BY count_24h DESC, signature
          LIMIT 3",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("ErrorMiner top open signatures: {e}"))?;
    let top_open = top_open
        .into_iter()
        .map(|(signature, state, count_24h)| OpenErrorSignature {
            signature,
            state,
            count_24h,
        })
        .collect();

    Ok(ErrorReport {
        available: true,
        reason: None,
        signatures_by_state,
        resolution_rate_30d,
        resolved_30d,
        resolution_denominator_30d: denominator_30d,
        mttr_hours_30d,
        current_week_mttr_hours,
        previous_week_mttr_hours,
        top_open,
        verdict: error_kpi_verdict(current_week_mttr_hours, previous_week_mttr_hours),
    })
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

/// Disk usage vs each worker's `disk_quota_pct`. Over quota = the node can no
/// longer accept model downloads and risks filling its root fs; approaching
/// quota (within 10 points) is an early warning the operator should prune.
fn disk_quota_health(over: i64, near: i64) -> Health {
    if over > 0 {
        Health::Fail
    } else if near > 0 {
        Health::Warn
    } else {
        Health::Pass
    }
}

/// Active model deployments (`desired_state = 'active'`) that are unhealthy or
/// whose health probe has gone stale. The reconciler re-adopts/restarts them,
/// but while degraded the gateway routes requests to a model that can't answer.
fn deployment_health(degraded: i64) -> Health {
    if degraded > 0 {
        Health::Warn
    } else {
        Health::Pass
    }
}

/// Recently-checked mesh edges (node→node SSH reachability in `fleet_mesh_status`)
/// that are NOT `ok`. A handful is transient and the leader-gated mesh-refresh
/// tick self-heals it; a wide spread means the fleet is fragmenting and
/// coordination (deploys, dispatch, failover) is unreliable.
fn mesh_health(failed: i64) -> Health {
    match failed {
        0 => Health::Pass,
        1..=10 => Health::Warn,
        _ => Health::Fail,
    }
}

/// Hours a session may sit `running`/`pending` before it counts as stuck.
/// Real sessions finish in minutes, so a multi-hour one is almost certainly
/// wedged (a step whose fleet_task hung/was deleted, a dead worker, or a
/// dependency deadlock like the one fixed in #602).
const STUCK_SESSION_HOURS: i32 = 6;

/// Sessions wedged in `running`/`pending` past [`STUCK_SESSION_HOURS`]. The
/// session orchestrator has no staleness watchdog, so these are otherwise
/// invisible until an operator goes looking. A non-zero count is a warning
/// (investigate / `ff session cancel`), not a fleet-fatal failure.
fn stuck_session_health(n: i64) -> Health {
    if n > 0 { Health::Warn } else { Health::Pass }
}

/// Leader liveness: a stale/absent leader heartbeat fails — leader-gated ticks
/// (autoscaler, reapers, upgrades) stop running without a fresh leader.
fn leader_health(fresh: bool) -> Health {
    if fresh { Health::Pass } else { Health::Fail }
}

/// Minutes a `'busy'` `sub_agents` slot may run before the stuck-slot reaper
/// (`sub_agent_reaper::BUSY_STALE_MINS`) assumes it's hung. Kept in lockstep
/// with that const so this check flags exactly what the reaper is about to
/// (or already would have) reset.
const STALE_BUSY_SLOT_MINS: i64 = 60;

/// Busy slots the stuck-slot reaper would reap (no active lease, past
/// [`STALE_BUSY_SLOT_MINS`]) — the direct symptom of a stuck/hung build, since
/// the reaper only runs every 10 minutes and a fleet operator wants to see the
/// count without waiting on that tick.
fn stale_slot_health(n: i64) -> Health {
    if n > 0 { Health::Warn } else { Health::Pass }
}

/// Processes stuck in uninterruptible sleep (`D` state).  A small number is
/// transient IO; a pileup (especially with idle CPU) is the classic signature
/// of a hard-mounted NFS peer going dark.
fn dstate_health(count: i64) -> Health {
    match count {
        0 => Health::Pass,
        1..=5 => Health::Warn,
        _ => Health::Fail,
    }
}

/// Peer mounts whose source node is unreachable on the SSH mesh.  These are
/// the mounts that will wedge in D-state if they are still hard-mounted.
fn stale_peer_mount_health(count: i64) -> Health {
    if count > 0 {
        Health::Warn
    } else {
        Health::Pass
    }
}

/// Render the report. Pure (no I/O / color in the assertions matter) so the
/// layout is unit-testable.
fn render_doctor(checks: &[DoctorCheck], errors: &ErrorReport, overall: Health) -> String {
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
    out.push_str("\n  ERRORS\n");
    if !errors.available {
        out.push_str(&format!(
            "    unavailable: {}\n",
            errors.reason.as_deref().unwrap_or("unknown reason")
        ));
    } else {
        out.push_str("    state        signatures\n");
        for (state, count) in &errors.signatures_by_state {
            out.push_str(&format!("    {state:<12} {count}\n"));
        }
        let rate = errors
            .resolution_rate_30d
            .map(|rate| format!("{:.1}%", rate * 100.0))
            .unwrap_or_else(|| "n/a".into());
        let mttr = errors
            .mttr_hours_30d
            .map(|hours| format!("{hours:.1}h"))
            .unwrap_or_else(|| "n/a".into());
        out.push_str(&format!(
            "    resolution rate (30d): {rate} ({}/{})  MTTR (30d): {mttr}\n",
            errors.resolved_30d, errors.resolution_denominator_30d
        ));
        out.push_str("    top open (24h):\n");
        if errors.top_open.is_empty() {
            out.push_str("      none\n");
        } else {
            for error in &errors.top_open {
                out.push_str(&format!(
                    "      {:>8}  {:<10} {}\n",
                    error.count_24h, error.state, error.signature
                ));
            }
        }
        out.push_str(&format!("    KPI: {:?}\n", errors.verdict).to_uppercase());
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

/// `ff doctor [--json] [--strict]`.
pub async fn handle_doctor(json: bool, strict: bool) -> Result<()> {
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

    // 5) Stuck sessions (running/pending past STUCK_SESSION_HOURS). The session
    //    orchestrator has no staleness watchdog, so a wedged session is
    //    otherwise invisible — this is its only health signal.
    let stuck_sessions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions
          WHERE status IN ('running', 'pending')
            AND COALESCE(started_at, created_at) < NOW() - make_interval(hours => $1)",
    )
    .bind(STUCK_SESSION_HOURS)
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("stuck-session check: {e}"))?;
    checks.push(DoctorCheck {
        name: "stuck sessions".into(),
        status: stuck_session_health(stuck_sessions),
        detail: format!("{stuck_sessions} running/pending >{STUCK_SESSION_HOURS}h"),
    });

    // 5b) Unit DSN-env lint (#44): a forgefleetd unit carrying a hardcoded
    //     FORGEFLEET_*_URL Environment= line re-arms the stale-DSN time bomb
    //     on the next reboot/upgrade (the July taylor-death class: 12 nodes
    //     silently pinned to a dead primary). Nodes must read the DSN from
    //     ~/.forgefleet/fleet.toml only.
    let (dsn_status, dsn_detail) = unit_dsn_env_lint();
    checks.push(DoctorCheck {
        name: "unit DSN env".into(),
        status: dsn_status,
        detail: dsn_detail,
    });

    // 6) Disk quota: latest sample per worker vs its disk_quota_pct. Stale
    //    samples (offline nodes) excluded via sampled_at.
    let (disk_over, disk_near): (i64, i64) = sqlx::query_as(
        "WITH latest AS (
             SELECT DISTINCT ON (worker_name) worker_name, used_bytes, total_bytes
               FROM fleet_disk_usage
              WHERE sampled_at > NOW() - INTERVAL '24 hours'
              ORDER BY worker_name, sampled_at DESC
         )
         SELECT
             COUNT(*) FILTER (
                 WHERE l.total_bytes > 0
                   AND l.used_bytes * 100.0 / l.total_bytes >= w.disk_quota_pct),
             COUNT(*) FILTER (
                 WHERE l.total_bytes > 0
                   AND l.used_bytes * 100.0 / l.total_bytes >= w.disk_quota_pct - 10
                   AND l.used_bytes * 100.0 / l.total_bytes <  w.disk_quota_pct)
           FROM latest l JOIN fleet_workers w ON w.name = l.worker_name",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("disk-quota check: {e}"))?;
    checks.push(DoctorCheck {
        name: "disk quota".into(),
        status: disk_quota_health(disk_over, disk_near),
        detail: format!("{disk_over} over / {disk_near} near quota"),
    });

    // 7) Mesh degradation: recently-checked node→node edges that aren't `ok`.
    //    Stale rows (offline nodes) are excluded via last_checked so this only
    //    flags live reachability loss, which breaks deploys/dispatch/failover.
    let mesh_failed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_mesh_status
          WHERE status <> 'ok'
            AND last_checked > NOW() - INTERVAL '1 hour'",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("mesh check: {e}"))?;
    checks.push(DoctorCheck {
        name: "mesh reachability".into(),
        status: mesh_health(mesh_failed),
        detail: format!("{mesh_failed} edges not ok (last 1h)"),
    });

    // 7c) Local D-state waiters.  Cheap /proc scan; non-Linux hosts report
    //     "not available" and pass.
    let dstate = ff_agent::shared_storage::local_dstate_waiter_count().unwrap_or(-1);
    checks.push(DoctorCheck {
        name: "D-state waiters".into(),
        status: if dstate < 0 {
            Health::Pass
        } else {
            dstate_health(dstate)
        },
        detail: if dstate < 0 {
            "not available on this OS".into()
        } else {
            format!("{dstate} processes in D-state")
        },
    });

    // 7d) Peer mounts whose source node has a recent failed mesh edge.  These
    //     are the mounts that will hang (or are hanging) when the peer dies.
    let stale_peer_mounts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM node_peer_mounts m
          JOIN computers c ON c.id = m.computer_id
          JOIN fleet_mesh_status s
            ON s.src_node = c.name AND s.dst_node = m.peer_name
         WHERE s.status = 'failed'
           AND s.last_checked > NOW() - INTERVAL '1 hour'",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("stale peer-mount check: {e}"))?;
    checks.push(DoctorCheck {
        name: "stale peer mounts".into(),
        status: stale_peer_mount_health(stale_peer_mounts),
        detail: format!("{stale_peer_mounts} peer mounts on failed mesh edges (last 1h)"),
    });

    // 7b) Model deployments that should be serving but are unhealthy/stale.
    let degraded_deployments: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_model_deployments
          WHERE desired_state = 'active'
            AND (health_status IS DISTINCT FROM 'healthy'
                 OR last_health_at IS NULL
                 OR last_health_at < NOW() - INTERVAL '15 minutes')",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("deployment-health check: {e}"))?;
    checks.push(DoctorCheck {
        name: "model deployments".into(),
        status: deployment_health(degraded_deployments),
        detail: format!("{degraded_deployments} active but unhealthy/stale"),
    });

    // 8) Leader liveness (fresh heartbeat within 60s).
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

    // 9) Stale busy slots: sub_agents rows stuck 'busy' with no active lease
    //    past the stuck-slot reaper's threshold — the direct signature of a
    //    hung/stuck build, surfaced here so an operator doesn't have to wait
    //    on the reaper's own 10-minute tick to see it.
    let stale_busy_slots: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sub_agents s
          WHERE s.status = 'busy'
            AND NOT EXISTS (
                 SELECT 1 FROM work_item_leases l
                  WHERE l.sub_agent_id = s.id AND l.released_at IS NULL)
            AND (s.started_at IS NULL
                 OR s.started_at < NOW() - make_interval(mins => $1))",
    )
    .bind(STALE_BUSY_SLOT_MINS as i32)
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("stale-busy-slot check: {e}"))?;
    checks.push(DoctorCheck {
        name: "stale busy slots".into(),
        status: stale_slot_health(stale_busy_slots),
        detail: format!(
            "{stale_busy_slots} busy slots stuck >{STALE_BUSY_SLOT_MINS}min, no active lease"
        ),
    });

    let errors = collect_error_report(&pool).await?;
    let overall = overall_health(&checks);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "overall": overall, "checks": checks, "errors": errors })
            )?
        );
    } else {
        print!("{}", render_doctor(&checks, &errors, overall));
    }

    // Non-zero exit so the loop / scripts / CI can gate on it: always on FAIL,
    // and on WARN too under --strict ("anything not green").
    if doctor_exit_code(overall, strict) != 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Whether service-unit text carries a hardcoded fleet DSN env line — the #44
/// pattern (`Environment=FORGEFLEET_POSTGRES_URL=…` etc.) that pins a node to
/// one primary's IP outside fleet.toml. Matches any FORGEFLEET_*_URL variable
/// on an Environment= line so future DSN vars are covered too. Pure for tests.
fn unit_text_has_dsn_env(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("Environment=") && line.contains("FORGEFLEET_") && line.contains("_URL=")
    })
}

/// Scan the local forgefleetd unit definitions (systemd user + system unit and
/// their drop-in dirs; the launchd plist on macOS) for hardcoded DSN env lines.
/// FAIL with the offending path(s) + a strip hint; PASS when clean or when no
/// unit exists at all (not-yet-onboarded box).
fn unit_dsn_env_lint() -> (Health, String) {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<std::path::PathBuf> = vec![
        format!("{home}/.config/systemd/user/forgefleetd.service").into(),
        "/etc/systemd/system/forgefleetd.service".into(),
        format!("{home}/Library/LaunchAgents/com.forgefleet.daemon.plist").into(),
    ];
    for dropin_dir in [
        format!("{home}/.config/systemd/user/forgefleetd.service.d"),
        "/etc/systemd/system/forgefleetd.service.d".to_string(),
    ] {
        if let Ok(entries) = std::fs::read_dir(&dropin_dir) {
            candidates.extend(entries.flatten().map(|e| e.path()));
        }
    }
    let offenders: Vec<String> = candidates
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok().map(|t| (p, t)))
        .filter(|(_, text)| unit_text_has_dsn_env(text))
        .map(|(p, _)| p.display().to_string())
        .collect();
    if offenders.is_empty() {
        (Health::Pass, "no hardcoded FORGEFLEET_*_URL env".into())
    } else {
        (
            Health::Fail,
            format!(
                "hardcoded DSN env in {} — strip the FORGEFLEET_*_URL Environment= line(s) and `systemctl daemon-reload`; the DSN belongs in ~/.forgefleet/fleet.toml",
                offenders.join(", ")
            ),
        )
    }
}

/// Process exit code for an overall verdict: FAIL always fails; WARN fails only
/// under `--strict`; PASS always succeeds. Pure, so the gating is unit-tested.
fn doctor_exit_code(overall: Health, strict: bool) -> i32 {
    match overall {
        Health::Fail => 1,
        Health::Warn if strict => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_exit_code_gates_on_warn() {
        // Default: only FAIL is non-zero.
        assert_eq!(doctor_exit_code(Health::Pass, false), 0);
        assert_eq!(doctor_exit_code(Health::Warn, false), 0);
        assert_eq!(doctor_exit_code(Health::Fail, false), 1);
        // --strict: WARN is non-zero too; PASS still succeeds.
        assert_eq!(doctor_exit_code(Health::Pass, true), 0);
        assert_eq!(doctor_exit_code(Health::Warn, true), 1);
        assert_eq!(doctor_exit_code(Health::Fail, true), 1);
    }

    #[test]
    fn unit_dsn_env_lint_matches_only_dsn_env_lines() {
        // The #44 pattern: hardcoded primary IP baked into the unit.
        assert!(unit_text_has_dsn_env(
            "[Service]\nEnvironment=FORGEFLEET_POSTGRES_URL=postgresql://ff@192.168.5.100:55432/ff\n"
        ));
        assert!(unit_text_has_dsn_env(
            "  Environment=FORGEFLEET_REDIS_URL=redis://192.168.5.100:56379\n"
        ));
        // Benign env the units legitimately carry must NOT trip the lint.
        assert!(!unit_text_has_dsn_env(
            "[Service]\nEnvironment=RUST_LOG=info\nEnvironment=FORGEFLEET_HOME=%h/.forgefleet\n"
        ));
        // Non-URL FORGEFLEET vars (e.g. FORGEFLEET_LEADER_HOST) are allowed.
        assert!(!unit_text_has_dsn_env(
            "Environment=FORGEFLEET_LEADER_HOST=192.168.5.104\n"
        ));
        // A DSN mentioned outside an Environment= line (comment) is fine.
        assert!(!unit_text_has_dsn_env(
            "# used to carry FORGEFLEET_POSTGRES_URL=… before #44\n"
        ));
    }

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
        assert_eq!(stuck_session_health(0), Health::Pass);
        assert_eq!(stuck_session_health(2), Health::Warn);
        assert_eq!(mesh_health(0), Health::Pass);
        assert_eq!(mesh_health(5), Health::Warn);
        assert_eq!(mesh_health(25), Health::Fail);
        assert_eq!(disk_quota_health(0, 0), Health::Pass);
        assert_eq!(disk_quota_health(0, 2), Health::Warn);
        assert_eq!(disk_quota_health(1, 0), Health::Fail);
        assert_eq!(disk_quota_health(1, 3), Health::Fail); // over wins over near
        assert_eq!(deployment_health(0), Health::Pass);
        assert_eq!(deployment_health(1), Health::Warn);
        assert_eq!(leader_health(true), Health::Pass);
        assert_eq!(leader_health(false), Health::Fail);
        assert_eq!(stale_slot_health(0), Health::Pass);
        assert_eq!(stale_slot_health(2), Health::Warn);
    }

    #[test]
    fn dstate_and_stale_peer_mount_thresholds() {
        assert_eq!(dstate_health(0), Health::Pass);
        assert_eq!(dstate_health(3), Health::Warn);
        assert_eq!(dstate_health(10), Health::Fail);
        assert_eq!(stale_peer_mount_health(0), Health::Pass);
        assert_eq!(stale_peer_mount_health(1), Health::Warn);
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
        let errors = ErrorReport {
            available: true,
            reason: None,
            signatures_by_state: BTreeMap::from([("resolved".into(), 4)]),
            resolution_rate_30d: Some(0.5),
            resolved_30d: 2,
            resolution_denominator_30d: 4,
            mttr_hours_30d: Some(12.0),
            current_week_mttr_hours: Some(10.0),
            previous_week_mttr_hours: Some(20.0),
            top_open: vec![OpenErrorSignature {
                signature: "timeout contacting worker".into(),
                state: "new".into(),
                count_24h: 7,
            }],
            verdict: ErrorKpiVerdict::Improving,
        };
        let out = render_doctor(&checks, &errors, overall_health(&checks));
        assert!(out.contains("ff doctor"));
        assert!(out.contains("deferred failures"));
        assert!(out.contains("leader liveness"));
        assert!(out.contains("Overall: FAIL"));
        assert!(out.contains("ERRORS"));
        assert!(out.contains("resolution rate (30d): 50.0% (2/4)"));
        assert!(out.contains("KPI: IMPROVING"));
    }

    #[test]
    fn error_kpi_compares_week_over_week_mttr() {
        assert_eq!(
            error_kpi_verdict(Some(9.0), Some(10.0)),
            ErrorKpiVerdict::Improving
        );
        assert_eq!(
            error_kpi_verdict(Some(11.0), Some(10.0)),
            ErrorKpiVerdict::Regressing
        );
        assert_eq!(
            error_kpi_verdict(Some(10.0), Some(10.0)),
            ErrorKpiVerdict::Flat
        );
        assert_eq!(error_kpi_verdict(None, Some(10.0)), ErrorKpiVerdict::Flat);
    }
}
