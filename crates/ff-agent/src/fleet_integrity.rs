//! Leader-gated fleet-integrity verify tick (PROD_READINESS item 23 — the
//! detection layer of the enrollment self-heal directive).
//!
//! ## Why
//! [`crate::verify_computer::verify_computer`] is a full post-onboarding check
//! battery (daemon health, DB reachability from the node, tool-version
//! reporting, defer-worker end-to-end, …). But it only ever ran **on demand**
//! — `ff fleet verify-node <n>` or the onboard gateway endpoint. So a host that
//! enrolled half-configured, OR drifted into a broken state *while still
//! alive*, stayed INVISIBLE until an operator manually re-verified it. That is
//! exactly the "9th identical half-configured box" the enrollment self-heal
//! memory calls out.
//!
//! [`crate::revive`]/`revive_scan` already covers the *dead* case (a node
//! ODOWN → restart its daemon / Wake-on-LAN). But an **alive-but-misconfigured**
//! node trips none of revive's liveness gates — nothing notices it.
//!
//! ## What this does
//! A leader-gated tick runs the verify battery across every **online** member
//! on a schedule and fires the `fleet_integrity_degraded` alert (warning,
//! telegram, 6h cooldown) when any member has failing checks. It is the
//! *detection* layer: it never mutates a target.
//!
//! ## Safety — gate `fleet_secrets.fleet_integrity_mode`
//! - `off` (DEFAULT): the tick is a no-op.
//! - `report` (alias `on`): run the sweep + alert on drift. **Never mutates.**
//! - `active`: run the sweep + alert, AND for each degraded node enqueue a SAFE
//!   per-gap auto-repair through the EXISTING deferred-task queue. The only gap
//!   that is auto-repairable today is the daemon-health/liveness check
//!   (`daemon_healthy`), which re-uses the existing `revive_member` task kind +
//!   handler — exactly what `leader_tick::revive_scan` enqueues for a dead node.
//!   Every other gap is recorded + alerted only (no mutation yet). The enqueue
//!   is leader-gated AND idempotent (it de-dupes against an in-flight
//!   `revive_member` task and against a recent audit row), so a flapping check
//!   cannot spam the queue.
//!
//! An unknown/missing value is treated as `off` (fail-safe), so deploying this
//! is harmless until an operator sets the secret to `active`. `active` does NOT
//! introduce a new repair executor: it only enqueues the already-shipped revive
//! task; non-liveness repairs deliberately remain alert-only.

use sqlx::{PgPool, Row};
use tracing::{info, warn};

use crate::verify_computer::{VerifyReport, verify_computer};

/// Fleet-secret key holding the three-mode gate (currently two live modes).
const FLEET_INTEGRITY_MODE_KEY: &str = "fleet_integrity_mode";

/// A member whose `last_seen_at` is within this window counts as "online" and
/// is therefore worth verifying. Matches the fleet's general liveness horizon;
/// a longer-dead node is `revive_scan`'s job, not ours.
const ONLINE_WINDOW: &str = "5 minutes";

/// Alert policy seeded by migration V131.
const POLICY_NAME: &str = "fleet_integrity_degraded";

/// The check name that, when failed, maps to a SAFE auto-repair (re-using the
/// existing `revive_member` deferred task). This is the daemon-health/liveness
/// probe in `verify_computer`. Kept as a single source of truth shared by the
/// pure [`repair_for_gap`] decision and its test.
const LIVENESS_CHECK: &str = "daemon_healthy";

/// The operating mode read from `fleet_secrets.fleet_integrity_mode` each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityMode {
    /// Tick does nothing (default — fail-safe).
    Off,
    /// Run the sweep and alert on drift; never mutate a target.
    Report,
    /// Run the sweep, alert on drift, AND enqueue SAFE per-gap auto-repairs
    /// (today: `revive_member` for a failed liveness check). Still leader-gated.
    Active,
}

impl IntegrityMode {
    /// Parse the raw secret value. Defaults to [`IntegrityMode::Off`] for
    /// `None`, empty, or any unrecognized value — the tick must never start
    /// doing work because a gate was mistyped.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => IntegrityMode::Active,
            Some("report") | Some("on") => IntegrityMode::Report,
            _ => IntegrityMode::Off,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            IntegrityMode::Off => "off",
            IntegrityMode::Report => "report",
            IntegrityMode::Active => "active",
        }
    }
}

/// The repair an active-mode tick will enqueue for a single failing check.
/// Pure + exhaustive so the "which gaps are auto-repairable" policy is
/// unit-testable without a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapRepair {
    /// Re-use the existing `revive_member` deferred task + handler.
    ReviveMember,
    /// Detected + alerted, but no safe auto-repair exists yet → record only.
    AlertOnly,
}

impl GapRepair {
    /// The `action` string recorded in `integrity_active_repairs`.
    pub fn as_str(self) -> &'static str {
        match self {
            GapRepair::ReviveMember => "revive_member",
            GapRepair::AlertOnly => "alert_only",
        }
    }
}

/// Pure decision: map a single failing check name to the SAFE repair (if any)
/// the active tick should take. Only the daemon-health/liveness check is
/// auto-repairable today — everything else is alert-only (recorded, not
/// mutated). Isolated so the auto-repair policy is unit-testable.
pub fn repair_for_gap(failing_check: &str) -> GapRepair {
    if failing_check == LIVENESS_CHECK {
        GapRepair::ReviveMember
    } else {
        GapRepair::AlertOnly
    }
}

/// Pure: does this degraded node have a liveness gap the active tick can
/// auto-repair? True iff any of its failing checks maps to a real repair.
pub fn node_is_auto_repairable(gaps: &NodeGaps) -> bool {
    gaps.failing_checks
        .iter()
        .any(|c| repair_for_gap(c) == GapRepair::ReviveMember)
}

/// The failing-check summary for a single degraded member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeGaps {
    pub node: String,
    pub failed: usize,
    /// Names of the checks that returned `fail` (skips are not gaps).
    pub failing_checks: Vec<String>,
}

/// The outcome of one sweep: every report gathered plus the degraded subset.
#[derive(Debug, Clone)]
pub struct IntegritySummary {
    pub checked: usize,
    pub degraded: Vec<NodeGaps>,
    pub reports: Vec<VerifyReport>,
}

/// Pure: derive the degraded-node list from a set of verify reports. A node is
/// degraded iff at least one check returned `fail` (a `skip` — e.g. a Windows
/// box that legitimately can't run a POSIX probe — is NOT a gap). Isolated so
/// the failure-mapping is unit-testable without SSH or a database.
pub fn degraded_from_reports(reports: &[VerifyReport]) -> Vec<NodeGaps> {
    reports
        .iter()
        .filter(|r| r.failed > 0)
        .map(|r| NodeGaps {
            node: r.node.clone(),
            failed: r.failed,
            failing_checks: r
                .details
                .iter()
                .filter(|c| c.status == "fail")
                .map(|c| c.check.clone())
                .collect(),
        })
        .collect()
}

/// Read the gate. Unreadable secret → `Off` (fail-safe), logged once.
async fn read_mode(pg: &PgPool) -> IntegrityMode {
    match ff_db::pg_get_secret(pg, FLEET_INTEGRITY_MODE_KEY).await {
        Ok(v) => IntegrityMode::parse(v.as_deref()),
        Err(e) => {
            warn!(error = %e, "fleet-integrity: gate read failed; treating as off");
            IntegrityMode::Off
        }
    }
}

/// List the names of currently-online members, excluding `my_name` (the leader
/// does not SSH-verify itself — its own health is covered by the local checks
/// and it is the one running this). Online == `computers.last_seen_at` within
/// [`ONLINE_WINDOW`].
async fn online_member_names(pg: &PgPool, my_name: &str) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT name FROM computers \
          WHERE name <> $1 \
            AND last_seen_at > NOW() - INTERVAL '{ONLINE_WINDOW}' \
          ORDER BY name"
    ))
    .bind(my_name)
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| r.try_get::<String, _>("name").ok())
        .collect())
}

/// Run the verify battery across every online member and return the summary.
/// Read-only: this never mutates a target and never alerts. Members are
/// verified **sequentially** — `verify_computer` SSHes into each host, and a
/// 15-node parallel SSH storm every tick is worse than a slightly longer
/// sequential sweep on a 15-minute cadence. A single node's verify error is
/// recorded as a synthetic failing report rather than aborting the sweep.
pub async fn run_integrity_sweep(
    pg: &PgPool,
    my_name: &str,
) -> Result<IntegritySummary, sqlx::Error> {
    let names = online_member_names(pg, my_name).await?;
    let mut reports = Vec::with_capacity(names.len());
    for name in names {
        match verify_computer(pg, &name).await {
            Ok(report) => reports.push(report),
            Err(e) => {
                warn!(node = %name, error = %e, "fleet-integrity: verify_computer errored");
                reports.push(VerifyReport {
                    node: name.clone(),
                    passed: 0,
                    failed: 1,
                    skipped: 0,
                    details: vec![crate::verify_computer::CheckResult {
                        check: "verify_battery_ran".into(),
                        status: "fail".into(),
                        message: Some(format!("verify_computer errored: {e}")),
                        retry_task_id: None,
                    }],
                    checked_at: chrono::Utc::now(),
                });
            }
        }
    }
    let degraded = degraded_from_reports(&reports);
    Ok(IntegritySummary {
        checked: reports.len(),
        degraded,
        reports,
    })
}

/// Fire the `fleet_integrity_degraded` alert through the seeded policy's
/// channel, then record the `alert_events` row — same shape as
/// [`crate::db_integrity`]. No-op if the policy is missing/disabled.
async fn fire_degraded_alert(pg: &PgPool, my_name: &str, degraded: &[NodeGaps]) {
    let policy: Option<(uuid::Uuid, String, String)> = match sqlx::query_as(
        "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
    )
    .bind(POLICY_NAME)
    .fetch_optional(pg)
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "fleet-integrity: failed to load {POLICY_NAME} policy");
            None
        }
    };
    let Some((policy_id, severity, channel)) = policy else {
        tracing::error!(
            "fleet-integrity: {} member(s) degraded but alert policy '{}' missing/disabled — NOT alerting",
            degraded.len(),
            POLICY_NAME
        );
        return;
    };

    let detail: Vec<String> = degraded
        .iter()
        .map(|g| format!("{} ({})", g.node, g.failing_checks.join(",")))
        .collect();
    let message = format!(
        "Fleet integrity: {} online member(s) failed the verify battery (detected by leader '{}'). \
         Run `ff fleet verify-node <name>` to inspect, then repair the specific gap. Degraded: {}",
        degraded.len(),
        my_name,
        detail.join("; ")
    );

    // Dispatch FIRST so the recorded channel_result reflects reality.
    let channel_result =
        crate::alert_evaluator::dispatch_alert(pg, &channel, &severity, &message).await;

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO alert_events
            (policy_id, computer_id, value, value_text, message, channel_result)
        VALUES ($1, NULL, $2, NULL, $3, $4)
        "#,
    )
    .bind(policy_id)
    .bind(degraded.len() as f64)
    .bind(&message)
    .bind(&channel_result)
    .execute(pg)
    .await
    {
        tracing::error!(error = %e, "fleet-integrity: failed to record alert_event");
    }

    warn!(
        degraded = degraded.len(),
        channel = %channel,
        channel_result = %channel_result,
        "fleet-integrity: degraded-member alert fired"
    );
}

/// Is a `revive_member` deferred task for `node` already pending/running? Mirrors
/// the de-dupe in [`crate::leader_tick`]'s `revive_scan` so the integrity tick and
/// the revive tick never double-enqueue the same revive.
async fn revive_inflight(pg: &PgPool, node: &str) -> bool {
    sqlx::query(
        "SELECT 1 FROM deferred_tasks
           WHERE kind = 'shell'
             AND status IN ('pending', 'dispatchable', 'running')
             AND title = $1",
    )
    .bind(format!("revive_member: {node}"))
    .fetch_optional(pg)
    .await
    .map(|r| r.is_some())
    .unwrap_or(false)
}

/// Did the active tick already enqueue a revive for `node` in the last 10 min?
/// Second idempotency guard (in case the deferred task already completed) so a
/// node that keeps flapping the liveness check is not revived every tick.
async fn repair_recently_audited(pg: &PgPool, node: &str) -> bool {
    sqlx::query(
        "SELECT 1 FROM integrity_active_repairs
           WHERE node = $1
             AND action = 'revive_member'
             AND created_at > NOW() - INTERVAL '10 minutes'",
    )
    .bind(node)
    .fetch_optional(pg)
    .await
    .map(|r| r.is_some())
    .unwrap_or(false)
}

/// Record one audit row in `integrity_active_repairs` (best-effort).
async fn record_repair(
    pg: &PgPool,
    node: &str,
    gap: &str,
    action: GapRepair,
    task_id: Option<&str>,
    leader: &str,
) {
    let task_uuid = task_id.and_then(|t| uuid::Uuid::parse_str(t).ok());
    if let Err(e) = sqlx::query(
        "INSERT INTO integrity_active_repairs (node, gap, action, deferred_task_id, leader)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(node)
    .bind(gap)
    .bind(action.as_str())
    .bind(task_uuid)
    .bind(leader)
    .execute(pg)
    .await
    {
        warn!(error = %e, node, "fleet-integrity: failed to record integrity_active_repairs row");
    }
}

/// Active mode: for every degraded node, enqueue the SAFE per-gap repair and
/// audit it. Today the ONLY auto-repair is `revive_member` for a failed liveness
/// check — it re-uses the existing revive task kind + handler (the exact enqueue
/// `leader_tick::revive_scan` performs), so no new repair executor is added here.
/// All other gaps are recorded as `alert_only`: detected + alerted, never
/// mutated. Returns the number of revive tasks actually enqueued.
async fn enqueue_active_repairs(pg: &PgPool, my_name: &str, degraded: &[NodeGaps]) -> usize {
    let mut enqueued = 0usize;
    for node_gaps in degraded {
        // Never act on ourselves (the leader does not revive itself).
        if node_gaps.node == my_name {
            continue;
        }
        for check in &node_gaps.failing_checks {
            match repair_for_gap(check) {
                GapRepair::ReviveMember => {
                    // Idempotency: skip if a revive is already in flight OR was
                    // enqueued for this node very recently.
                    if revive_inflight(pg, &node_gaps.node).await
                        || repair_recently_audited(pg, &node_gaps.node).await
                    {
                        tracing::debug!(
                            node = %node_gaps.node,
                            "fleet-integrity active: revive already in-flight/recent — skipping"
                        );
                        continue;
                    }

                    // Re-use the EXISTING revive_member task shape verbatim.
                    let title = format!("revive_member: {}", node_gaps.node);
                    let script = format!("ff fleet revive {} --internal", node_gaps.node);
                    let payload = serde_json::json!({ "command": script });
                    let trigger_spec = serde_json::json!({});
                    let required_caps = serde_json::json!([]);

                    match ff_db::queries::pg_enqueue_deferred(
                        pg,
                        &title,
                        "shell",
                        &payload,
                        "now",
                        &trigger_spec,
                        Some(my_name),
                        &required_caps,
                        Some(&format!("fleet-integrity:{my_name}")),
                        Some(2),
                    )
                    .await
                    {
                        Ok(id) => {
                            info!(
                                node = %node_gaps.node,
                                task_id = %id,
                                gap = %check,
                                "fleet-integrity active: enqueued revive_member repair"
                            );
                            record_repair(
                                pg,
                                &node_gaps.node,
                                check,
                                GapRepair::ReviveMember,
                                Some(&id),
                                my_name,
                            )
                            .await;
                            enqueued += 1;
                        }
                        Err(e) => warn!(
                            node = %node_gaps.node,
                            error = %e,
                            "fleet-integrity active: failed to enqueue revive_member repair"
                        ),
                    }
                }
                GapRepair::AlertOnly => {
                    // No safe auto-repair yet — record that we saw it and moved on.
                    record_repair(
                        pg,
                        &node_gaps.node,
                        check,
                        GapRepair::AlertOnly,
                        None,
                        my_name,
                    )
                    .await;
                }
            }
        }
    }
    enqueued
}

/// One full tick body: gate → sweep → alert (→ active repair). Returns the
/// summary (or `None` when gated off) so callers/tests can assert on it.
pub async fn run_once(pg: &PgPool, my_name: &str) -> Option<IntegritySummary> {
    let mode = read_mode(pg).await;
    if mode == IntegrityMode::Off {
        return None;
    }
    let summary = match run_integrity_sweep(pg, my_name).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "fleet-integrity: sweep failed");
            return None;
        }
    };
    if summary.degraded.is_empty() {
        info!(
            mode = mode.as_str(),
            checked = summary.checked,
            "fleet-integrity: all online members healthy"
        );
    } else {
        fire_degraded_alert(pg, my_name, &summary.degraded).await;
        // Only `active` mutates — and only via the existing revive task.
        if mode == IntegrityMode::Active {
            let enqueued = enqueue_active_repairs(pg, my_name, &summary.degraded).await;
            info!(
                mode = mode.as_str(),
                degraded = summary.degraded.len(),
                revives_enqueued = enqueued,
                "fleet-integrity active: per-gap repair pass complete"
            );
        }
    }
    Some(summary)
}

/// Spawn the leader-gated fleet-integrity loop. The skip path reads the
/// process-local leader cache, so this is safe to start on every daemon.
pub fn spawn_fleet_integrity_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }

                    run_once(&pg, &worker_name).await;
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("fleet-integrity tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_computer::CheckResult;

    fn report(node: &str, checks: &[(&str, &str)]) -> VerifyReport {
        let details: Vec<CheckResult> = checks
            .iter()
            .map(|(c, s)| CheckResult {
                check: (*c).into(),
                status: (*s).into(),
                message: None,
                retry_task_id: None,
            })
            .collect();
        let passed = details.iter().filter(|c| c.status == "pass").count();
        let failed = details.iter().filter(|c| c.status == "fail").count();
        let skipped = details.iter().filter(|c| c.status == "skip").count();
        VerifyReport {
            node: node.into(),
            passed,
            failed,
            skipped,
            details,
            checked_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn mode_defaults_off_and_is_failsafe() {
        assert_eq!(IntegrityMode::parse(None), IntegrityMode::Off);
        assert_eq!(IntegrityMode::parse(Some("")), IntegrityMode::Off);
        assert_eq!(IntegrityMode::parse(Some("garbage")), IntegrityMode::Off);
        assert_eq!(IntegrityMode::parse(Some("off")), IntegrityMode::Off);
        // A near-miss must NOT silently enable active mutation.
        assert_eq!(IntegrityMode::parse(Some("activ")), IntegrityMode::Off);
        assert_eq!(
            IntegrityMode::parse(Some("active-repair")),
            IntegrityMode::Off
        );
    }

    #[test]
    fn mode_parses_report_aliases_case_insensitively() {
        assert_eq!(IntegrityMode::parse(Some("report")), IntegrityMode::Report);
        assert_eq!(IntegrityMode::parse(Some("REPORT")), IntegrityMode::Report);
        assert_eq!(IntegrityMode::parse(Some(" on ")), IntegrityMode::Report);
    }

    #[test]
    fn mode_parses_active_case_insensitively_and_roundtrips() {
        assert_eq!(IntegrityMode::parse(Some("active")), IntegrityMode::Active);
        assert_eq!(IntegrityMode::parse(Some("ACTIVE")), IntegrityMode::Active);
        assert_eq!(
            IntegrityMode::parse(Some(" Active ")),
            IntegrityMode::Active
        );
        assert_eq!(IntegrityMode::Off.as_str(), "off");
        assert_eq!(IntegrityMode::Report.as_str(), "report");
        assert_eq!(IntegrityMode::Active.as_str(), "active");
    }

    #[test]
    fn only_liveness_gap_is_auto_repairable() {
        // The daemon-health/liveness check maps to the existing revive task.
        assert_eq!(repair_for_gap("daemon_healthy"), GapRepair::ReviveMember);
        // Every other known gap is alert-only (no safe auto-repair yet).
        for other in [
            "db_reachable",
            "tool_versions_reported",
            "mesh_ssh_complete",
            "sudo_passwordless",
            "openclaw_registered",
            "defer_end_to_end",
            "library_health",
            "verify_battery_ran",
            "",
        ] {
            assert_eq!(
                repair_for_gap(other),
                GapRepair::AlertOnly,
                "{other} must NOT be auto-repaired"
            );
        }
        assert_eq!(GapRepair::ReviveMember.as_str(), "revive_member");
        assert_eq!(GapRepair::AlertOnly.as_str(), "alert_only");
    }

    #[test]
    fn node_auto_repairable_iff_it_has_a_liveness_gap() {
        let live = NodeGaps {
            node: "dead".into(),
            failed: 2,
            failing_checks: vec!["db_reachable".into(), "daemon_healthy".into()],
        };
        assert!(node_is_auto_repairable(&live));

        let drift_only = NodeGaps {
            node: "drifted".into(),
            failed: 2,
            failing_checks: vec!["db_reachable".into(), "mesh_ssh_complete".into()],
        };
        assert!(!node_is_auto_repairable(&drift_only));

        let no_gaps = NodeGaps {
            node: "clean".into(),
            failed: 0,
            failing_checks: vec![],
        };
        assert!(!node_is_auto_repairable(&no_gaps));
    }

    #[test]
    fn degraded_only_includes_nodes_with_a_real_failure() {
        let reports = vec![
            report(
                "healthy",
                &[("daemon_healthy", "pass"), ("db_reachable", "pass")],
            ),
            report(
                "skip-only",
                &[("daemon_healthy", "pass"), ("win_probe", "skip")],
            ),
            report(
                "broken",
                &[("daemon_healthy", "fail"), ("db_reachable", "pass")],
            ),
        ];
        let degraded = degraded_from_reports(&reports);
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].node, "broken");
        assert_eq!(degraded[0].failed, 1);
        assert_eq!(degraded[0].failing_checks, vec!["daemon_healthy"]);
    }

    #[test]
    fn degraded_lists_all_failing_checks_for_a_node() {
        let reports = vec![report(
            "messy",
            &[
                ("daemon_healthy", "fail"),
                ("db_reachable", "fail"),
                ("tool_versions", "pass"),
            ],
        )];
        let degraded = degraded_from_reports(&reports);
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].failed, 2);
        assert_eq!(
            degraded[0].failing_checks,
            vec!["daemon_healthy", "db_reachable"]
        );
    }
}
