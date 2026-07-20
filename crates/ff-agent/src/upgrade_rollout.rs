//! Staged upgrade rollout + auto-halt — the leader-gated `upgrade_rollout` tick
//! (PROD_READINESS item 26). Phase 1 of `plans/staged-upgrade-rollout.md`.
//!
//! ## Why
//! Today `task_runner::compose_fleet_upgrade_wave` composes EVERY non-leader
//! target into priority-ordered waves and inserts them ALL AT ONCE. Priority
//! gates ORDER, not SUCCESS — nothing stops wave N+1 from running after wave N
//! FAILED, so one bad build rolls all 14 non-leader hosts before failures
//! surface (the documented wave self-kill history).
//!
//! ## What this does
//! Replaces "dispatch all waves at once" with a GATED progression. A rollout
//! row (`upgrade_rollouts`) carries an ordered `stages` list
//! (`[{stage_idx, target_names[]}]`). Stage 0 (the canary, usually 1 follower)
//! is composed up front by `ff fleet rollout start --staged`. Every 60s this
//! leader-gated tick, for each `in_progress` rollout:
//!   1. counts the CURRENT stage's `fleet_tasks` by status,
//!   2. if any are still running → does nothing (stage in flight),
//!   3. if ALL terminal → computes the failure rate and decides:
//!      - breach (canary: ≥1 fail; later stages: failed/total > threshold) →
//!        `status='halted'` + `halted_reason`, fire the `upgrade_rollout_halted`
//!        alert, and WITHHOLD every remaining stage,
//!      - else → advance `current_stage`; compose ONLY the next stage's targets
//!        (preserving the V62 one-wave-per-family invariant), or
//!        `status='completed'` when no stages remain.
//!
//! The halt DECISION (`decide_stage`) is a pure function so it is unit-tested
//! without a database. Restart tasks restore `forgefleetd.prev` on failed
//! verification; this tick then halts the rollout and alerts.
//!
//! ## Safety — gate `fleet_secrets.rollout_mode`
//! `manual` (the default) permits explicit staged rollouts but creates none;
//! `auto` lets the leader create a rollout once merge/time drift crosses the
//! threshold. Existing rollouts progress in either mode so flipping back to
//! manual never strands an in-flight safety operation.
//!
//! Mirrors the other leader ticks for the leader gate: rollout state is global,
//! so only the leader advances it (no N-way compose races). On failover the new
//! leader's forgefleetd picks the tick up.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tracing::{info, warn};

/// Operator gate for continuous convergence. Missing/invalid is manual.
const ROLLOUT_MODE_KEY: &str = "rollout_mode";
const AUTO_MERGE_THRESHOLD: usize = 3;
const AUTO_AGE_THRESHOLD_SECS: i64 = 15 * 60;
const CANARY_BAKE_SECS: i64 = 10 * 60;

/// Alert policy seeded by migration V134.
const POLICY_NAME: &str = "upgrade_rollout_halted";

/// Wave fanout used when composing a stage's targets. The stage IS the
/// concurrency unit, so a generous fanout lets a whole stage build in parallel.
const STAGE_FANOUT: usize = 8;

/// The operating mode read from `fleet_secrets.rollout_mode` each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutMode {
    Manual,
    Auto,
}

impl RolloutMode {
    /// Parse the raw secret value. `None`, empty, or any unrecognised value →
    /// [`RolloutMode::Manual`] — the tick must never start actuating because a gate
    /// was mistyped.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("auto") => RolloutMode::Auto,
            _ => RolloutMode::Manual,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RolloutMode::Manual => "manual",
            RolloutMode::Auto => "auto",
        }
    }
}

/// One stage in a rollout: an ordered subset of member names to upgrade together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutStage {
    pub stage_idx: usize,
    pub target_names: Vec<String>,
}

/// Terminal-outcome tallies for a single stage's `fleet_tasks`, used by the pure
/// [`decide_stage`] decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageTally {
    /// Tasks in this stage that reached `completed`.
    pub completed: usize,
    /// Tasks in this stage that reached `failed` or `cancelled`.
    pub failed: usize,
    /// Tasks still `pending`/`running` (non-terminal).
    pub running: usize,
}

impl StageTally {
    fn total_terminal(&self) -> usize {
        self.completed + self.failed
    }
}

/// The decision the gate reaches for one stage. Pure — derived only from the
/// tally, whether this stage is the canary (stage 0), the failure threshold, and
/// whether a further stage exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageDecision {
    /// Stage still has non-terminal tasks — wait.
    Wait,
    /// Stage passed; advance to the next stage and compose it.
    Advance,
    /// Stage passed and was the last one — the rollout is complete.
    Complete,
    /// Stage's failure rate breached the threshold — halt the rollout.
    Halt { failed: usize, total: usize },
}

/// Pure stage-gate decision.
///
/// - If any task is still running → [`StageDecision::Wait`].
/// - If all terminal, compute the breach:
///   - the **canary** stage (`is_canary`, i.e. stage 0) halts on the FIRST
///     failure (`failed >= 1`) — a 1-host canary that fails is 100% and a bad
///     build must never pass it,
///   - a later stage halts when `failed * 100 / total > failure_threshold_pct`.
/// - On a breach → [`StageDecision::Halt`].
/// - Otherwise advance: [`StageDecision::Complete`] if this was the last stage,
///   else [`StageDecision::Advance`].
///
/// `total == 0` (no tasks for the stage — e.g. every target was unresolvable)
/// is treated as a pass-through advance, never a halt: there is nothing to gate
/// on, and stalling the rollout forever on an empty stage is worse than moving
/// past it.
pub fn decide_stage(
    tally: StageTally,
    is_canary: bool,
    failure_threshold_pct: i32,
    has_more_stages: bool,
) -> StageDecision {
    if tally.running > 0 {
        return StageDecision::Wait;
    }
    let total = tally.total_terminal();
    if total == 0 {
        // Empty stage — nothing to gate; advance/complete.
        return if has_more_stages {
            StageDecision::Advance
        } else {
            StageDecision::Complete
        };
    }
    let breach = if is_canary {
        tally.failed >= 1
    } else {
        let pct = failure_threshold_pct.max(0) as usize;
        // failed/total > threshold%  ⇔  failed*100 > total*threshold
        tally.failed.saturating_mul(100) > total.saturating_mul(pct)
    };
    if breach {
        StageDecision::Halt {
            failed: tally.failed,
            total,
        }
    } else if has_more_stages {
        StageDecision::Advance
    } else {
        StageDecision::Complete
    }
}

/// Read the gate. Unreadable secret → `Manual` (fail-safe), logged once.
async fn read_mode(pg: &PgPool) -> RolloutMode {
    match ff_db::pg_read_gate_value(pg, ROLLOUT_MODE_KEY, "manual", "manual").await {
        Ok(v) => RolloutMode::parse(Some(v.as_str())),
        Err(e) => {
            warn!(error = %e, "continuous-rollout: gate read failed; treating as manual");
            RolloutMode::Manual
        }
    }
}

pub async fn continuous_mode_is_auto(pg: &PgPool) -> bool {
    read_mode(pg).await == RolloutMode::Auto
}

/// A live rollout row (only the columns the tick needs).
#[derive(Debug, Clone)]
struct RolloutRow {
    id: uuid::Uuid,
    software_id: String,
    stages: Vec<RolloutStage>,
    current_stage: i32,
    failure_threshold_pct: i32,
    canary_bake_started_at: Option<chrono::DateTime<chrono::Utc>>,
    automatic: bool,
}

/// Load every `in_progress` rollout (oldest first for stable ordering).
async fn load_in_progress(pg: &PgPool) -> Result<Vec<RolloutRow>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT id, COALESCE(software_id, '') AS software_id,
               COALESCE(stages, '[]'::jsonb) AS stages,
               current_stage, failure_threshold_pct, canary_bake_started_at, automatic
          FROM upgrade_rollouts
         WHERE status = 'in_progress'
         ORDER BY created_at ASC
        "#,
    )
    .fetch_all(pg)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let stages_json: serde_json::Value = r.try_get("stages").unwrap_or(serde_json::json!([]));
        let stages: Vec<RolloutStage> = serde_json::from_value(stages_json).unwrap_or_default();
        out.push(RolloutRow {
            id: r.try_get("id")?,
            software_id: r.try_get("software_id")?,
            stages,
            current_stage: r.try_get("current_stage")?,
            failure_threshold_pct: r.try_get("failure_threshold_pct")?,
            canary_bake_started_at: r.try_get("canary_bake_started_at")?,
            automatic: r.try_get("automatic")?,
        });
    }
    Ok(out)
}

/// Count the current stage's `fleet_tasks` by terminal class.
async fn tally_stage(
    pg: &PgPool,
    rollout_id: uuid::Uuid,
    stage: i32,
) -> Result<StageTally, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT
          COUNT(*) FILTER (WHERE status = 'completed')                  AS completed,
          COUNT(*) FILTER (WHERE status IN ('failed', 'cancelled'))     AS failed,
          COUNT(*) FILTER (WHERE status NOT IN ('completed','failed','cancelled')) AS running
          FROM fleet_tasks
         WHERE rollout_id = $1 AND rollout_stage = $2
        "#,
    )
    .bind(rollout_id)
    .bind(stage)
    .fetch_one(pg)
    .await?;
    Ok(StageTally {
        completed: row.try_get::<i64, _>("completed").unwrap_or(0) as usize,
        failed: row.try_get::<i64, _>("failed").unwrap_or(0) as usize,
        running: row.try_get::<i64, _>("running").unwrap_or(0) as usize,
    })
}

/// Canary promotion requires a full bake window after restart, a fresh daemon
/// beat, and proof that every canary subsequently claimed and completed real
/// fleet work. This catches binaries that merely start but cannot build.
async fn canary_bake_passed(pg: &PgPool, rollout: &RolloutRow) -> Result<bool, sqlx::Error> {
    let Some(stage) = rollout.stages.first() else {
        return Ok(false);
    };
    let Some(started) = rollout.canary_bake_started_at else {
        sqlx::query(
            "UPDATE upgrade_rollouts SET canary_bake_started_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND canary_bake_started_at IS NULL",
        )
        .bind(rollout.id)
        .execute(pg)
        .await?;
        return Ok(false);
    };
    if chrono::Utc::now()
        .signed_duration_since(started)
        .num_seconds()
        < CANARY_BAKE_SECS
    {
        return Ok(false);
    }
    let ready: bool = sqlx::query_scalar(
        r#"
        SELECT NOT EXISTS (
            SELECT 1
              FROM computers c
             WHERE c.name = ANY($1::text[])
               AND (
                    c.last_seen_at IS NULL OR c.last_seen_at < $2
                    OR NOT EXISTS (
                        SELECT 1
                          FROM work_item_leases l
                          JOIN work_items w ON w.id = l.work_item_id
                         WHERE l.computer_id = c.id
                           AND l.created_at >= $2
                           AND w.status IN ('done','merged')
                    )
               )
        )
        "#,
    )
    .bind(&stage.target_names)
    .bind(started)
    .fetch_one(pg)
    .await?;
    Ok(ready)
}

/// Resolve the leader's `computers.id` (rollouts always exclude the leader).
async fn leader_computer_id(pg: &PgPool, my_name: &str) -> Result<uuid::Uuid, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
        .bind(my_name)
        .fetch_one(pg)
        .await
}

/// Compose a single stage's targets into an upgrade wave and tag every task it
/// enqueued with `rollout_id` / `rollout_stage` so the gate can count the stage.
/// Reuses `compose_fleet_upgrade_wave_filtered` so the SSH build/restart
/// machinery (V52 two-phase, V108 per-host deps) is identical to the unstaged
/// path; only the target set differs.
pub async fn compose_stage(
    pg: &PgPool,
    software_id: &str,
    rollout_id: uuid::Uuid,
    stage_idx: i32,
    target_names: &[String],
    leader_id: uuid::Uuid,
) -> Result<usize, String> {
    if target_names.is_empty() {
        return Ok(0);
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM work_item_leases l \
         JOIN computers c ON c.id = l.computer_id \
         WHERE c.name = ANY($1::text[]) AND l.released_at IS NULL)",
    )
    .bind(target_names)
    .fetch_one(pg)
    .await
    .map_err(|e| format!("check rollout target leases: {e}"))?;
    if busy {
        return Ok(0);
    }
    let plan = crate::task_runner::compose_fleet_upgrade_wave_filtered(
        pg,
        software_id,
        STAGE_FANOUT,
        leader_id,
        false,
        Some(target_names),
    )
    .await
    .map_err(|e| format!("compose stage {stage_idx}: {e}"))?;

    let Some(parent) = plan.parent else {
        return Ok(0);
    };
    // Tag the parent + all its children with the rollout id/stage so the gate
    // can tally them. The compose path keys everything off `parent_task_id`.
    let tagged = sqlx::query(
        r#"
        UPDATE fleet_tasks
           SET rollout_id = $1, rollout_stage = $2
         WHERE id = $3 OR parent_task_id = $3
        "#,
    )
    .bind(rollout_id)
    .bind(stage_idx)
    .bind(parent)
    .execute(pg)
    .await
    .map_err(|e| format!("tag rollout tasks: {e}"))?;
    Ok(tagged.rows_affected() as usize)
}

/// Pure drift trigger used by the leader tick.
pub fn drift_exceeds_threshold(merges: usize, age_secs: i64) -> bool {
    merges >= AUTO_MERGE_THRESHOLD || age_secs >= AUTO_AGE_THRESHOLD_SECS
}

/// Start one automatic daemon rollout, if the DB gate and merge/time drift
/// threshold permit it. The first stage contains exactly one canary from each
/// architecture; the second contains the remaining followers. A partial unique
/// index provides the cross-leader singleton during failover.
pub async fn maybe_start_continuous_rollout(
    pg: &PgPool,
    software_id: &str,
    my_name: &str,
    running_sha: &str,
    target_sha: &str,
) -> Result<bool, String> {
    if read_mode(pg).await != RolloutMode::Auto || running_sha.trim().is_empty() {
        return Ok(false);
    }
    let source_tree: Option<String> =
        sqlx::query_scalar("SELECT source_tree_path FROM computers WHERE lower(name) = lower($1)")
            .bind(my_name)
            .fetch_optional(pg)
            .await
            .map_err(|e| format!("continuous rollout source tree: {e}"))?
            .flatten();
    let Some(mut source_tree) = source_tree else {
        return Ok(false);
    };
    if let Some(rest) = source_tree.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        source_tree = format!("{home}/{rest}");
    }
    let count = tokio::process::Command::new("git")
        .args([
            "-C",
            &source_tree,
            "rev-list",
            "--merges",
            "--count",
            &format!("{running_sha}..{target_sha}"),
        ])
        .output()
        .await
        .map_err(|e| format!("count merge drift: {e}"))?;
    let merges = String::from_utf8_lossy(&count.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0);
    let shown = tokio::process::Command::new("git")
        .args(["-C", &source_tree, "show", "-s", "--format=%ct", target_sha])
        .output()
        .await
        .map_err(|e| format!("read target age: {e}"))?;
    let committed_at = String::from_utf8_lossy(&shown.stdout)
        .trim()
        .parse::<i64>()
        .unwrap_or(i64::MAX);
    let age_secs = chrono::Utc::now().timestamp().saturating_sub(committed_at);
    if !count.status.success()
        || !shown.status.success()
        || !drift_exceeds_threshold(merges, age_secs)
    {
        return Ok(false);
    }

    let rows = sqlx::query(
        r#"
        SELECT c.name,
               COALESCE(c.metadata->>'arch', c.build_archs->>0, c.os_family, 'unknown') AS arch
          FROM computers c
          JOIN computer_software cs ON cs.computer_id = c.id
         WHERE cs.software_id = $1
           AND lower(c.name) <> lower($2)
           AND c.status = 'online'
           AND COALESCE(c.reservation_state, 'available') = 'available'
         ORDER BY arch, c.name
        "#,
    )
    .bind(software_id)
    .bind(my_name)
    .fetch_all(pg)
    .await
    .map_err(|e| format!("select continuous rollout targets: {e}"))?;
    let mut by_arch: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for row in rows {
        by_arch
            .entry(row.try_get("arch").unwrap_or_else(|_| "unknown".into()))
            .or_default()
            .push(row.try_get("name").map_err(|e| e.to_string())?);
    }
    let mut canaries = Vec::new();
    let mut remaining = Vec::new();
    for names in by_arch.values() {
        if let Some((first, rest)) = names.split_first() {
            canaries.push(first.clone());
            remaining.extend_from_slice(rest);
        }
    }
    if canaries.is_empty() {
        return Ok(false);
    }
    let mut stages = vec![RolloutStage {
        stage_idx: 0,
        target_names: canaries,
    }];
    if !remaining.is_empty() {
        stages.push(RolloutStage {
            stage_idx: 1,
            target_names: remaining,
        });
    }
    let stages_json = serde_json::to_value(&stages).map_err(|e| e.to_string())?;
    let rollout_id: uuid::Uuid = match sqlx::query_scalar(
        r#"INSERT INTO upgrade_rollouts
              (software_id, started_by, stages, current_stage, status,
               failure_threshold_pct, target_version, automatic)
            VALUES ($1, $2, $3, 0, 'in_progress', 0, $4, TRUE)
            RETURNING id"#,
    )
    .bind(software_id)
    .bind(my_name)
    .bind(stages_json)
    .bind(target_sha)
    .fetch_one(pg)
    .await
    {
        Ok(id) => id,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Ok(false),
        Err(e) => return Err(format!("insert continuous rollout: {e}")),
    };
    let leader_id = leader_computer_id(pg, my_name)
        .await
        .map_err(|e| format!("continuous rollout leader: {e}"))?;
    compose_stage(
        pg,
        software_id,
        rollout_id,
        0,
        &stages[0].target_names,
        leader_id,
    )
    .await?;
    info!(%rollout_id, merges, age_secs, architectures = stages[0].target_names.len(),
          "continuous-rollout: automatic canary wave started");
    Ok(true)
}

/// Create a staged rollout row and compose ONLY stage 0 (the canary). Stages
/// after the canary are recorded in the row but composed lazily by the tick as
/// each prior stage passes. `available_targets` is the resolvable non-leader
/// member set (already excluding the leader); `canary` is the canary size.
///
/// Returns the new rollout id. Used by `ff fleet rollout start --staged`.
pub async fn create_staged_rollout(
    pg: &PgPool,
    software_id: &str,
    available_targets: &[String],
    canary: usize,
    failure_threshold_pct: i32,
    started_by: &str,
) -> Result<uuid::Uuid, String> {
    let stages = plan_stages(available_targets, canary);
    if stages.is_empty() {
        return Err("no resolvable non-leader targets for this software".into());
    }
    let stages_json =
        serde_json::to_value(&stages).map_err(|e| format!("serialize stages: {e}"))?;

    let rollout_id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO upgrade_rollouts
            (software_id, started_by, stages, current_stage, status, failure_threshold_pct)
        VALUES ($1, $2, $3, 0, 'in_progress', $4)
        RETURNING id
        "#,
    )
    .bind(software_id)
    .bind(started_by)
    .bind(&stages_json)
    .bind(failure_threshold_pct.max(0))
    .fetch_one(pg)
    .await
    .map_err(|e| format!("insert upgrade_rollouts: {e}"))?;

    let leader_id = leader_computer_id(pg, started_by)
        .await
        .map_err(|e| format!("resolve leader computer id: {e}"))?;

    // Compose ONLY stage 0.
    let tagged = compose_stage(
        pg,
        software_id,
        rollout_id,
        0,
        &stages[0].target_names,
        leader_id,
    )
    .await?;
    info!(
        rollout_id = %rollout_id,
        software_id = %software_id,
        stages = stages.len(),
        canary_targets = stages[0].target_names.len(),
        tagged,
        "staged-rollout: created + composed canary stage 0"
    );
    Ok(rollout_id)
}

/// Pure: split the available target list into ordered stages — a canary of
/// `canary` hosts (clamped to ≥1 and ≤ len) followed by a single "the rest"
/// stage (Phase 1's two-stage shape). An empty target list yields no stages.
pub fn plan_stages(available_targets: &[String], canary: usize) -> Vec<RolloutStage> {
    if available_targets.is_empty() {
        return Vec::new();
    }
    let canary = canary.clamp(1, available_targets.len());
    let mut stages = vec![RolloutStage {
        stage_idx: 0,
        target_names: available_targets[..canary].to_vec(),
    }];
    if canary < available_targets.len() {
        stages.push(RolloutStage {
            stage_idx: 1,
            target_names: available_targets[canary..].to_vec(),
        });
    }
    stages
}

/// Phase 2: percentage-staged plan. Stage 0 is the canary; subsequent stages
/// each grow coverage to the next cumulative percentage of ALL targets
/// (e.g. `--stages 10,50,100` → canary, then up-to-10%, up-to-50%, up-to-100%).
/// Percentages are clamped to 1..=100, sorted ascending, deduped to non-empty
/// slices, and a final 100% slice is always appended so every host is covered.
/// Empty `pcts` falls back to [`plan_stages`] (canary + the rest). Pure +
/// unit-tested; the tick advances `current_stage` through whatever this returns.
pub fn plan_stages_pct(
    available_targets: &[String],
    canary: usize,
    pcts: &[u8],
) -> Vec<RolloutStage> {
    if available_targets.is_empty() {
        return Vec::new();
    }
    if pcts.is_empty() {
        return plan_stages(available_targets, canary);
    }
    let n = available_targets.len();
    let canary = canary.clamp(1, n);
    let mut stages = vec![RolloutStage {
        stage_idx: 0,
        target_names: available_targets[..canary].to_vec(),
    }];

    // Cumulative cut points (host counts) from the percentages, always ending at n.
    let mut cuts: Vec<usize> = pcts
        .iter()
        .map(|p| {
            let p = (*p).clamp(1, 100) as usize;
            // ceil(p% of n), never before the canary so a stage is non-empty.
            ((p * n).div_ceil(100)).clamp(canary, n)
        })
        .collect();
    cuts.push(n);
    cuts.sort_unstable();
    cuts.dedup();

    let mut prev = canary;
    for cut in cuts {
        if cut > prev {
            stages.push(RolloutStage {
                stage_idx: stages.len(),
                target_names: available_targets[prev..cut].to_vec(),
            });
            prev = cut;
        }
    }
    stages
}

/// Fire the `upgrade_rollout_halted` alert through the seeded policy's channel,
/// then record the `alert_events` row — same shape as
/// [`crate::fleet_integrity`] / `db_integrity`. No-op if the policy is
/// missing/disabled.
async fn fire_halt_alert(
    pg: &PgPool,
    my_name: &str,
    rollout: &RolloutRow,
    failed: usize,
    total: usize,
) {
    let policy: Option<(uuid::Uuid, String, String)> = match sqlx::query_as(
        "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
    )
    .bind(POLICY_NAME)
    .fetch_optional(pg)
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "staged-rollout: failed to load {POLICY_NAME} policy");
            None
        }
    };
    let Some((policy_id, severity, channel)) = policy else {
        tracing::error!(
            "staged-rollout: rollout {} halted but alert policy '{}' missing/disabled — NOT alerting",
            rollout.id,
            POLICY_NAME
        );
        return;
    };

    let message = format!(
        "Staged upgrade rollout HALTED: software '{}' (rollout {}) — stage {} had {}/{} task(s) fail, \
         crossing the failure threshold (detected by leader '{}'). Remaining stages were withheld. \
         Inspect with `ff fleet rollout status`, then repair the build and consider rolling back the \
         affected host(s) — rollback is operator-driven (updates are never auto-applied).",
        rollout.software_id, rollout.id, rollout.current_stage, failed, total, my_name
    );

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
    .bind(failed as f64)
    .bind(&message)
    .bind(&channel_result)
    .execute(pg)
    .await
    {
        tracing::error!(error = %e, "staged-rollout: failed to record alert_event");
    }

    warn!(
        rollout_id = %rollout.id,
        software_id = %rollout.software_id,
        stage = rollout.current_stage,
        failed,
        total,
        channel = %channel,
        channel_result = %channel_result,
        "staged-rollout: halt alert fired"
    );
}

/// Per-rollout summary of what the tick did (for the log + tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutAction {
    pub rollout_id: uuid::Uuid,
    pub decision: StageDecision,
}

/// Evaluate every `in_progress` rollout once. Reads the gate; off = no-op.
/// In `dry-run` it logs the decision and actuates nothing. In `active` it
/// applies the decision (advance/halt/complete + alert + compose). Returns the
/// per-rollout actions (empty when gated off) so callers/tests can assert.
pub async fn run_once(pg: &PgPool, my_name: &str) -> Vec<RolloutAction> {
    let mode = read_mode(pg).await;

    let rollouts = match load_in_progress(pg).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "staged-rollout: failed to load in-progress rollouts");
            return Vec::new();
        }
    };

    let mut actions = Vec::new();
    for r in &rollouts {
        let stage = r.current_stage;
        let tally = match tally_stage(pg, r.id, stage).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, rollout_id = %r.id, "staged-rollout: tally failed");
                continue;
            }
        };
        let has_more = (stage as usize + 1) < r.stages.len();
        let is_canary = stage == 0;
        if r.automatic && tally.total_terminal() == 0 && tally.running == 0 {
            let targets = r
                .stages
                .get(stage as usize)
                .map(|s| s.target_names.clone())
                .unwrap_or_default();
            if let Ok(leader_id) = leader_computer_id(pg, my_name).await {
                match compose_stage(pg, &r.software_id, r.id, stage, &targets, leader_id).await {
                    Ok(0) => info!(rollout_id = %r.id, stage,
                                  "continuous-rollout: stage targets busy; revisiting"),
                    Ok(n) => info!(rollout_id = %r.id, stage, tagged = n,
                                  "continuous-rollout: deferred stage composed"),
                    Err(e) => warn!(rollout_id = %r.id, stage, error = %e,
                                    "continuous-rollout: deferred stage compose failed"),
                }
            }
            actions.push(RolloutAction {
                rollout_id: r.id,
                decision: StageDecision::Wait,
            });
            continue;
        }
        let mut decision = decide_stage(tally, is_canary, r.failure_threshold_pct, has_more);
        if is_canary && matches!(decision, StageDecision::Advance | StageDecision::Complete) {
            match canary_bake_passed(pg, r).await {
                Ok(true) => {}
                Ok(false) => decision = StageDecision::Wait,
                Err(e) => {
                    warn!(error = %e, rollout_id = %r.id, "continuous-rollout: canary bake evidence unavailable");
                    decision = StageDecision::Wait;
                }
            }
        }

        info!(
            rollout_id = %r.id,
            software_id = %r.software_id,
            stage,
            completed = tally.completed,
            failed = tally.failed,
            running = tally.running,
            decision = ?decision,
            mode = mode.as_str(),
            "staged-rollout: stage evaluated"
        );

        actions.push(RolloutAction {
            rollout_id: r.id,
            decision,
        });

        // Both modes progress already-created rollouts; `manual` only prevents
        // the leader tick from creating a new automatic rollout.
        match decision {
            StageDecision::Wait => {}
            StageDecision::Halt { failed, total } => {
                let reason = format!(
                    "stage {stage}: {failed}/{total} task(s) failed (threshold {}%{})",
                    r.failure_threshold_pct,
                    if is_canary {
                        ", canary: any failure"
                    } else {
                        ""
                    }
                );
                if let Err(e) = sqlx::query(
                    "UPDATE upgrade_rollouts \
                       SET status = 'halted', halted_reason = $2, updated_at = NOW() \
                     WHERE id = $1 AND status = 'in_progress'",
                )
                .bind(r.id)
                .bind(&reason)
                .execute(pg)
                .await
                {
                    warn!(error = %e, rollout_id = %r.id, "staged-rollout: failed to set halted");
                    continue;
                }
                fire_halt_alert(pg, my_name, r, failed, total).await;
            }
            StageDecision::Complete => {
                if let Err(e) = sqlx::query(
                    "UPDATE upgrade_rollouts \
                       SET status = 'completed', updated_at = NOW() \
                     WHERE id = $1 AND status = 'in_progress'",
                )
                .bind(r.id)
                .execute(pg)
                .await
                {
                    warn!(error = %e, rollout_id = %r.id, "staged-rollout: failed to set completed");
                }
            }
            StageDecision::Advance => {
                let next = stage + 1;
                let targets = r
                    .stages
                    .get(next as usize)
                    .map(|s| s.target_names.clone())
                    .unwrap_or_default();
                let leader_id = match leader_computer_id(pg, my_name).await {
                    Ok(id) => id,
                    Err(e) => {
                        warn!(error = %e, "staged-rollout: leader id lookup failed; cannot compose next stage");
                        continue;
                    }
                };
                match compose_stage(pg, &r.software_id, r.id, next, &targets, leader_id).await {
                    Ok(n) if n > 0 => {
                        if let Err(e) = sqlx::query(
                            "UPDATE upgrade_rollouts SET current_stage = $2, updated_at = NOW() \
                             WHERE id = $1 AND status = 'in_progress' AND current_stage = $3",
                        )
                        .bind(r.id)
                        .bind(next)
                        .bind(stage)
                        .execute(pg)
                        .await
                        {
                            warn!(error = %e, rollout_id = %r.id, "staged-rollout: failed to advance stage");
                        } else {
                            info!(rollout_id = %r.id, stage = next, tagged = n,
                                  "staged-rollout: composed next stage");
                        }
                    }
                    Ok(_) => info!(rollout_id = %r.id, stage = next,
                                  "continuous-rollout: targets busy; will revisit after leases drain"),
                    Err(e) => warn!(
                        error = %e, rollout_id = %r.id, stage = next,
                        "staged-rollout: next-stage compose failed (will retry next tick)"
                    ),
                }
            }
        }
    }
    actions
}

/// Spawn the leader-gated staged-rollout loop. Leadership is checked inside the
/// loop on every fire (not at spawn), exactly like the other leader ticks, so
/// this is safe to start on every daemon.
pub fn spawn_upgrade_rollout_tick(
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
        info!("staged-rollout tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_manual_and_is_failsafe() {
        assert_eq!(RolloutMode::parse(None), RolloutMode::Manual);
        assert_eq!(RolloutMode::parse(Some("")), RolloutMode::Manual);
        assert_eq!(RolloutMode::parse(Some("garbage")), RolloutMode::Manual);
        assert_eq!(RolloutMode::parse(Some("manual")), RolloutMode::Manual);
    }

    #[test]
    fn mode_parses_auto() {
        assert_eq!(RolloutMode::parse(Some(" AUTO ")), RolloutMode::Auto);
        assert_eq!(RolloutMode::Manual.as_str(), "manual");
        assert_eq!(RolloutMode::Auto.as_str(), "auto");
    }

    #[test]
    fn continuous_drift_uses_merge_or_time_threshold() {
        assert!(!drift_exceeds_threshold(2, 899));
        assert!(drift_exceeds_threshold(3, 0));
        assert!(drift_exceeds_threshold(0, 900));
    }

    fn tally(completed: usize, failed: usize, running: usize) -> StageTally {
        StageTally {
            completed,
            failed,
            running,
        }
    }

    #[test]
    fn wait_while_any_task_still_running() {
        // Even with a failure already, a non-terminal task means WAIT.
        let d = decide_stage(tally(1, 1, 2), true, 25, true);
        assert_eq!(d, StageDecision::Wait);
    }

    #[test]
    fn canary_halts_on_first_failure() {
        // Canary (stage 0): a single failure with no running tasks halts,
        // even though 1/2 = 50% < a percentage threshold would normally allow.
        let d = decide_stage(tally(1, 1, 0), true, 25, true);
        assert_eq!(
            d,
            StageDecision::Halt {
                failed: 1,
                total: 2
            }
        );
    }

    #[test]
    fn canary_passes_when_all_completed() {
        let d = decide_stage(tally(1, 0, 0), true, 25, true);
        assert_eq!(d, StageDecision::Advance);
    }

    #[test]
    fn non_canary_tolerates_failures_under_threshold() {
        // 1 of 10 failed = 10% <= 25% threshold → advance (more stages exist).
        let d = decide_stage(tally(9, 1, 0), false, 25, true);
        assert_eq!(d, StageDecision::Advance);
    }

    #[test]
    fn non_canary_halts_above_threshold() {
        // 3 of 10 failed = 30% > 25% threshold → halt.
        let d = decide_stage(tally(7, 3, 0), false, 25, true);
        assert_eq!(
            d,
            StageDecision::Halt {
                failed: 3,
                total: 10
            }
        );
    }

    #[test]
    fn threshold_is_strict_greater_than() {
        // Exactly at the threshold (25% of 8 = 2) must NOT halt.
        let d = decide_stage(tally(6, 2, 0), false, 25, true);
        assert_eq!(d, StageDecision::Advance);
        // One more failure (3/8 = 37.5%) halts.
        let d = decide_stage(tally(5, 3, 0), false, 25, true);
        assert_eq!(
            d,
            StageDecision::Halt {
                failed: 3,
                total: 8
            }
        );
    }

    #[test]
    fn last_stage_pass_completes() {
        let d = decide_stage(tally(10, 0, 0), false, 25, false);
        assert_eq!(d, StageDecision::Complete);
    }

    #[test]
    fn last_stage_breach_still_halts() {
        let d = decide_stage(tally(0, 5, 0), false, 25, false);
        assert_eq!(
            d,
            StageDecision::Halt {
                failed: 5,
                total: 5
            }
        );
    }

    #[test]
    fn empty_stage_advances_not_halts() {
        // No tasks at all (every target unresolvable) → don't stall; pass through.
        assert_eq!(
            decide_stage(tally(0, 0, 0), true, 25, true),
            StageDecision::Advance
        );
        assert_eq!(
            decide_stage(tally(0, 0, 0), false, 25, false),
            StageDecision::Complete
        );
    }

    #[test]
    fn plan_stages_canary_then_rest() {
        let targets: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let stages = plan_stages(&targets, 1);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].target_names, vec!["a".to_string()]);
        assert_eq!(
            stages[1].target_names,
            vec!["b".to_string(), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn plan_stages_canary_clamped_and_single_stage_when_all_canary() {
        let targets: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        // canary >= len → one stage covering everything (no "rest").
        let stages = plan_stages(&targets, 5);
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].target_names.len(), 2);
        // canary 0 clamps to 1.
        let stages = plan_stages(&targets, 0);
        assert_eq!(stages[0].target_names, vec!["a".to_string()]);
    }

    #[test]
    fn plan_stages_empty_targets_yields_nothing() {
        assert!(plan_stages(&[], 1).is_empty());
    }

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("h{i}")).collect()
    }

    #[test]
    fn plan_stages_pct_builds_cumulative_percentage_stages() {
        // 10 hosts, canary 1, stages 10/50/100 → canary(1) + up-to-10%(1) +
        // up-to-50%(5) + up-to-100%(10). Cumulative cuts at host counts 1,5,10.
        let t = names(10);
        let stages = plan_stages_pct(&t, 1, &[10, 50, 100]);
        let sizes: Vec<usize> = stages.iter().map(|s| s.target_names.len()).collect();
        // canary(1) IS the 10% cut (1 host) so that stage collapses; then +4 to
        // 50% (5 hosts) and +5 to 100% (10 hosts).
        assert_eq!(sizes, vec![1, 4, 5]);
        // every host covered exactly once, idx contiguous
        let total: usize = sizes.iter().sum();
        assert_eq!(total, 10);
        for (i, s) in stages.iter().enumerate() {
            assert_eq!(s.stage_idx, i);
        }
    }

    #[test]
    fn plan_stages_pct_empty_pcts_falls_back_to_canary_then_rest() {
        let t = names(6);
        assert_eq!(plan_stages_pct(&t, 2, &[]), plan_stages(&t, 2));
    }

    #[test]
    fn plan_stages_pct_dedups_and_always_covers_all() {
        // Duplicate/garbage percentages collapse; a final 100% slice is always
        // present so no host is ever stranded un-upgraded.
        let t = names(4);
        let stages = plan_stages_pct(&t, 1, &[50, 50, 200]); // 200 clamps to 100
        let total: usize = stages.iter().map(|s| s.target_names.len()).sum();
        assert_eq!(total, 4);
        assert_eq!(stages.last().unwrap().target_names.last().unwrap(), "h3");
    }
}
