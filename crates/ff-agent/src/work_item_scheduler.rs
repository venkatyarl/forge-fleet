//! Pillar 4 — distributed concurrent development scheduler tick.
//!
//! Leader-only, serial. Each tick: reap stale leases (freeing slots + returning
//! their work_items to the ready pool), then assign `status='ready'` work_items
//! to free fleet slots via [`ff_db::pg_assign_work_item`] (one active lease per
//! item, enforced by a partial-unique index). Single-leader serial execution
//! means no cross-process race — no `FOR UPDATE SKIP LOCKED` needed.
//!
//! v1 = assignment (lease + slot reservation). The slot's agent loop picks up
//! its `current_work_item_id` to execute; the merge-queue drain + dispatch are
//! follow-ups. Only touches work_items explicitly flagged `status='ready'`, so
//! operator PM items (status 'idea' etc.) are never disturbed.
//!
//! Design: `.forgefleet/plans/DECISION-pillar4-canonical-home.md`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet, VecDeque};
use tracing::{info, warn};

/// Lower bound for the heartbeat-stale lease window. The actual scheduler value
/// is measured from recent successful build leases by [`lease_stale_secs`].
///
/// The floor also gives `work_item_dispatch::LANE1_TIMEOUT_SECS` a compile-time
/// bound: Lane-1 local codegen must self-abort before even the smallest reaper
/// window can reclaim a live lease.
pub(crate) const MIN_LEASE_STALE_SECS: i64 = 480;
/// Upper bound for the data-derived heartbeat-stale lease window. This stays
/// below [`MAX_LEASE_DURATION_SECS`], which remains the separate hard age cap
/// for leases whose heartbeat keeps refreshing while the build is wedged.
const MAX_LEASE_STALE_SECS: i64 = 2400;
const LEASE_STALE_MIN_SAMPLES: i64 = 20;
const LEASE_STALE_SAMPLE_DAYS: i64 = 30;
/// Safety margin over the measured p99 successful build duration.
const LEASE_STALE_P99_MARGIN_NUMERATOR: i64 = 5;
const LEASE_STALE_P99_MARGIN_DENOMINATOR: i64 = 4;
/// Hard ceiling on lease HOLD time regardless of heartbeat — reclaims a wedged
/// dispatch that keeps its heartbeat fresh but makes no progress (the
/// "building forever, live heartbeat" wedge). Above a real build's Lane-2 cap
/// (~18.5 min).
pub(crate) const MAX_LEASE_DURATION_SECS: i64 = 45 * 60;
/// Lease lifetime granted at assignment (refreshed by heartbeats).
pub(crate) const LEASE_GRANT_SECS: i64 = 600;
/// Max assignments per tick (back-pressure; the rest wait for the next tick).
const MAX_ASSIGN_PER_TICK: i64 = 64;
/// Minimum age before an `in_progress` work_item with NO active lease is
/// considered orphaned and cancelled. Far above the lease/heartbeat windows so
/// a legitimately-leased item is never swept mid-assignment.
const ORPHAN_MIN_AGE_SECS: i64 = 3600;
/// Failure-convergence ceiling: after this many stalled/reaped attempts a
/// work_item is marked `failed` (with context) instead of re-queued forever.
/// A task the swarm genuinely can't build must STOP thrashing and surface for a
/// human or a retry-with-error-context.
///
/// MUST stay STRICTLY ABOVE `ff_routing_policy::LOCAL_LANE_MAX_TRIES` (=3): the
/// escalation ladder keeps a build on the local Devstral lane for the first
/// LOCAL_LANE_MAX_TRIES attempts, then escalates to cloud (claude/codex). If this
/// cap equals LOCAL_LANE_MAX_TRIES the item dies on the LAST local attempt and
/// cloud NEVER gets a try (root cause of the 2026-07-22 "53 items failed after 3
/// stalled attempts, zero cloud escalation" freeze). 5 = 3 local + 2 cloud tries.
const MAX_BUILD_ATTEMPTS: i32 = 5;
/// Four missed 15-second dispatch passes makes a host ineligible. General
/// Pulse beats may still be fresh when this subsystem clock is stale.
const DISPATCH_TICK_STALE_SECS: i64 = 60;
const FAILED_RETRY_COOLDOWN_MINUTES: i64 = 20;
const MAX_FAILED_RETRIES: i32 = 3;
pub(crate) const WORK_ITEM_EXECUTION_ENABLED_KEY: &str = "work_item_execution_enabled";
const WORK_ITEM_EXECUTION_DEFAULT: bool = true;
const WORK_ITEM_EXECUTION_RESTORE_ON_EXPIRY: bool = true;

fn resolve_work_item_execution_gate<E>(result: std::result::Result<bool, E>) -> bool
where
    E: std::fmt::Display,
{
    match result {
        Ok(enabled) => enabled,
        Err(error) => {
            warn!(
                key = WORK_ITEM_EXECUTION_ENABLED_KEY,
                %error,
                "work-item execution gate read failed; refusing new execution"
            );
            false
        }
    }
}

/// Whether the autonomous Pillar-4 pipeline may assign or start new work.
///
/// Missing rows preserve the historical enabled behavior. Temporary disables
/// restore to enabled when their TTL expires. A read failure is fail-closed so
/// recovery work cannot accidentally resume while gate authority is unknown.
pub(crate) async fn work_item_execution_enabled(pg: &PgPool) -> bool {
    resolve_work_item_execution_gate(
        ff_db::pg_read_safety_gate(
            pg,
            WORK_ITEM_EXECUTION_ENABLED_KEY,
            WORK_ITEM_EXECUTION_DEFAULT,
            WORK_ITEM_EXECUTION_RESTORE_ON_EXPIRY,
        )
        .await,
    )
}

const LEASE_STALE_SAMPLE_SQL: &str = r#"
SELECT COUNT(*) FILTER (
           WHERE build_started_at IS NOT NULL
             AND released_at > build_started_at
       )::bigint AS native_sample_count,
       percentile_cont(0.99) WITHIN GROUP (
           ORDER BY EXTRACT(EPOCH FROM (released_at - build_started_at))
       ) FILTER (
           WHERE build_started_at IS NOT NULL
             AND released_at > build_started_at
       ) AS native_p99_secs,
       COUNT(*) FILTER (
           WHERE build_started_at IS NULL
             AND dispatch_tick_at IS NOT NULL
             AND released_at > dispatch_tick_at
       )::bigint AS bootstrap_sample_count,
       percentile_cont(0.99) WITHIN GROUP (
           ORDER BY EXTRACT(EPOCH FROM (released_at - dispatch_tick_at))
       ) FILTER (
           WHERE build_started_at IS NULL
             AND dispatch_tick_at IS NOT NULL
             AND released_at > dispatch_tick_at
       ) AS bootstrap_p99_secs
  FROM work_item_leases
 WHERE release_reason = 'ready for review'
   AND released_at <= NOW()
   AND released_at >= NOW() - make_interval(days => $1)
"#;

const AUTO_REQUEUE_FAILED_SQL: &str = r#"
WITH eligible AS (
    SELECT w.id
      FROM work_items w
     WHERE w.status = 'failed'
       AND w.kind = 'task'
       AND w.retry_count < $1
       AND w.completed_at <= NOW() - make_interval(mins => $2)
       AND COALESCE(w.last_error, '') !~* '(^|[^A-Z])(BOGUS|QUARANTINE|CANCELLED)([^A-Z]|$)'
       AND NOT EXISTS (
           SELECT 1 FROM work_item_leases l
            WHERE l.work_item_id = w.id AND l.released_at IS NULL
       )
     ORDER BY w.completed_at ASC, w.id ASC
     FOR UPDATE SKIP LOCKED
     LIMIT $3
)
UPDATE work_items w
   SET status = 'ready',
       retry_count = w.retry_count + 1,
       attempts = GREATEST(w.attempts, $4),
       assigned_to = NULL,
       assigned_computer = NULL,
       completed_at = NULL
  FROM eligible
 WHERE w.id = eligible.id
"#;

/// Claim deployed cloud-fixes-local remediations and rebuild the original item
/// with a fresh local-first attempt budget. The diagnosis row is the durable
/// state machine, so concurrent leader ticks cannot enqueue the same retest
/// twice.
const REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL: &str = r#"
WITH claimed AS (
    UPDATE local_failure_diagnoses d
       SET remediation_status = 'local_retest_running',
           local_retest_started_at = NOW(),
           local_retest_completed_at = NULL,
           local_retest_error = NULL,
           updated_at = NOW()
     WHERE d.id IN (
        SELECT d2.id
          FROM local_failure_diagnoses d2
          JOIN work_items w ON w.id = d2.work_item_id
         WHERE d2.remediation_status = 'deployed'
           AND d2.deployed_at IS NOT NULL
           AND NOT EXISTS (
               SELECT 1
                 FROM local_failure_diagnoses newer
                WHERE newer.work_item_id = d2.work_item_id
                  AND newer.remediation_status = 'deployed'
                  AND newer.rescue_attempt > d2.rescue_attempt
           )
           AND w.status IN ('done', 'failed', 'merged')
           AND NOT EXISTS (
               SELECT 1 FROM work_item_leases l
                WHERE l.work_item_id = w.id AND l.released_at IS NULL
           )
         ORDER BY d2.created_at, d2.id
         FOR UPDATE OF d2 SKIP LOCKED
         LIMIT $1
     )
 RETURNING d.work_item_id
)
UPDATE work_items w
   SET status = 'ready',
       attempts = 0,
       assigned_to = NULL,
       assigned_computer = NULL,
       completed_at = NULL,
       last_error = NULL
  FROM claimed
 WHERE w.id = claimed.work_item_id
"#;

/// Materialize diagnosed local failures in the existing improvement stores.
/// The diagnosis row and its derived artifact advance in one transaction, so a
/// scheduler restart cannot duplicate training or context-pack inputs.
const ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL: &str = r#"
WITH claimed AS MATERIALIZED (
    SELECT d.id, d.work_item_id, d.local_failure_summary, d.cloud_diagnosis,
           d.cause_class, d.improvement_route
      FROM local_failure_diagnoses d
     WHERE d.remediation_status = 'diagnosed'
       AND d.improvement_route IN ('dreamer_context_pack', 'fine_tune_model_ab')
     ORDER BY d.created_at, d.id
     FOR UPDATE SKIP LOCKED
     LIMIT $1
),
context_sources AS (
    INSERT INTO local_context_sources (uri, title, source_type, metadata)
    SELECT 'local-failure-diagnosis://' || c.id,
           'Cloud rescue diagnosis for work item ' || c.work_item_id,
           'note',
           jsonb_build_object(
               'local_failure_diagnosis_id', c.id,
               'work_item_id', c.work_item_id,
               'cause_class', c.cause_class,
               'improvement_route', c.improvement_route
           )
      FROM claimed c
     WHERE c.improvement_route = 'dreamer_context_pack'
    RETURNING id, metadata
),
context_chunks AS (
    INSERT INTO local_context_chunks (source_id, chunk_index, content, metadata)
    SELECT s.id, 0,
           c.local_failure_summary || E'\n\nCloud diagnosis: ' || c.cloud_diagnosis,
           s.metadata
      FROM context_sources s
      JOIN claimed c
        ON c.id = (s.metadata->>'local_failure_diagnosis_id')::uuid
    RETURNING source_id
),
context_updates AS (
    UPDATE work_items w
       SET context = COALESCE(w.context, '{}'::jsonb) ||
           jsonb_build_object(
               'local_failure_improvement',
               jsonb_build_object(
                   'diagnosis_id', c.id,
                   'cause_class', c.cause_class,
                   'local_failure_summary', c.local_failure_summary,
                   'cloud_diagnosis', c.cloud_diagnosis
               )
           )
      FROM claimed c
     WHERE w.id = c.work_item_id
       AND c.improvement_route = 'dreamer_context_pack'
       AND EXISTS (SELECT 1 FROM context_chunks)
    RETURNING w.id
),
training_inputs AS (
    INSERT INTO training_jobs
        (name, training_data_path, training_type, params, created_by)
    SELECT 'local-failure-' || c.id,
           'local-failure-diagnosis://' || c.id,
           'lora',
           jsonb_build_object(
               'local_failure_diagnosis_id', c.id,
               'work_item_id', c.work_item_id,
               'local_failure_summary', c.local_failure_summary,
               'cloud_diagnosis', c.cloud_diagnosis,
               'cause_class', c.cause_class
           ),
           'cloud-fixes-local'
      FROM claimed c
     WHERE c.improvement_route = 'fine_tune_model_ab'
    RETURNING id
)
UPDATE local_failure_diagnoses d
   SET remediation_status = 'deploy_pending',
       updated_at = NOW()
  FROM claimed c
 WHERE d.id = c.id
   AND (
       (c.improvement_route = 'dreamer_context_pack'
        AND EXISTS (SELECT 1 FROM context_updates))
       OR
       (c.improvement_route = 'fine_tune_model_ab'
        AND EXISTS (SELECT 1 FROM training_inputs))
   )
"#;

/// Advance an improvement only when its artifact is available to the local
/// lane. Context-pack chunks live in the shared database and are immediately
/// deployed; capability fixes additionally require a completed training job
/// whose resulting model is healthy and active somewhere in the fleet.
const RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL: &str = r#"
WITH eligible AS MATERIALIZED (
    SELECT d.id
      FROM local_failure_diagnoses d
     WHERE d.remediation_status = 'deploy_pending'
       AND (
           (
               d.improvement_route = 'dreamer_context_pack'
               AND EXISTS (
                   SELECT 1
                     FROM local_context_sources s
                     JOIN local_context_chunks c ON c.source_id = s.id
                    WHERE s.uri = 'local-failure-diagnosis://' || d.id
               )
           )
           OR
           (
               d.improvement_route = 'fine_tune_model_ab'
               AND EXISTS (
                   SELECT 1
                     FROM training_jobs t
                    WHERE t.params->>'local_failure_diagnosis_id' = d.id::text
                      AND t.status = 'completed'
                      AND NULLIF(t.result_model_id, '') IS NOT NULL
                      AND EXISTS (
                          SELECT 1
                            FROM computers target
                           WHERE target.status = 'online'
                             AND target.has_gpu
                             AND target.reservation_state <> 'drained'
                      )
                      AND NOT EXISTS (
                          SELECT 1
                            FROM computers target
                           WHERE target.status = 'online'
                             AND target.has_gpu
                             AND target.reservation_state <> 'drained'
                             AND NOT (
                          EXISTS (
                              SELECT 1
                                FROM fleet_model_deployments f
                               WHERE f.catalog_id = t.result_model_id
                                 AND f.worker_name = target.name
                                 AND f.health_status = 'healthy'
                                 AND f.desired_state = 'active'
                          )
                          OR EXISTS (
                              SELECT 1
                                FROM computer_model_deployments c
                               WHERE c.model_id = t.result_model_id
                                 AND c.computer_id = target.id
                                 AND c.status = 'active'
                          )
                      )
                      )
               )
           )
       )
     ORDER BY d.created_at, d.id
     FOR UPDATE OF d SKIP LOCKED
     LIMIT $1
)
UPDATE local_failure_diagnoses d
   SET remediation_status = 'deployed',
       deployed_at = COALESCE(d.deployed_at, NOW()),
       updated_at = NOW()
  FROM eligible e
 WHERE d.id = e.id
   AND d.remediation_status = 'deploy_pending'
"#;

fn lease_stale_secs_from_success_p99(sample_count: i64, p99_secs: Option<f64>) -> i64 {
    if sample_count < LEASE_STALE_MIN_SAMPLES {
        return MIN_LEASE_STALE_SECS;
    }
    let Some(p99_secs) = p99_secs else {
        return MIN_LEASE_STALE_SECS;
    };
    if !p99_secs.is_finite() || p99_secs <= 0.0 {
        return MIN_LEASE_STALE_SECS;
    }

    let with_margin = (p99_secs * LEASE_STALE_P99_MARGIN_NUMERATOR as f64
        / LEASE_STALE_P99_MARGIN_DENOMINATOR as f64)
        .ceil() as i64;
    with_margin.clamp(MIN_LEASE_STALE_SECS, MAX_LEASE_STALE_SECS)
}

fn select_lease_stale_sample(
    native_sample_count: i64,
    native_p99_secs: Option<f64>,
    bootstrap_sample_count: i64,
    bootstrap_p99_secs: Option<f64>,
) -> (&'static str, i64, Option<f64>) {
    let native_valid = native_p99_secs.is_some_and(|p99| p99.is_finite() && p99 > 0.0);
    if native_sample_count >= LEASE_STALE_MIN_SAMPLES && native_valid {
        (
            "released_at-build_started_at",
            native_sample_count,
            native_p99_secs,
        )
    } else {
        (
            "released_at-dispatch_tick_at-bootstrap",
            bootstrap_sample_count,
            bootstrap_p99_secs,
        )
    }
}

pub(crate) async fn lease_stale_secs(pg: &PgPool) -> i64 {
    let row = sqlx::query(LEASE_STALE_SAMPLE_SQL)
        .bind(LEASE_STALE_SAMPLE_DAYS as i32)
        .fetch_one(pg)
        .await;

    match row {
        Ok(row) => {
            let native_sample_count: i64 = row.get("native_sample_count");
            let native_p99_secs: Option<f64> = row.try_get("native_p99_secs").ok().flatten();
            let bootstrap_sample_count: i64 = row.get("bootstrap_sample_count");
            let bootstrap_p99_secs: Option<f64> = row.try_get("bootstrap_p99_secs").ok().flatten();
            let (duration_basis, sample_count, p99_secs) = select_lease_stale_sample(
                native_sample_count,
                native_p99_secs,
                bootstrap_sample_count,
                bootstrap_p99_secs,
            );
            let stale_secs = lease_stale_secs_from_success_p99(sample_count, p99_secs);
            info!(
                duration_basis,
                sample_count,
                p99_secs,
                native_sample_count,
                bootstrap_sample_count,
                stale_secs,
                min_samples = LEASE_STALE_MIN_SAMPLES,
                sample_days = LEASE_STALE_SAMPLE_DAYS,
                "work_item_scheduler: measured lease-stale heartbeat window"
            );
            stale_secs
        }
        Err(e) => {
            warn!(
                error = %e,
                fallback_secs = MIN_LEASE_STALE_SECS,
                "work_item_scheduler: failed to measure lease-stale window; using floor"
            );
            MIN_LEASE_STALE_SECS
        }
    }
}

async fn auto_requeue_failed_work_items(pg: &PgPool) -> Result<u64> {
    Ok(sqlx::query(AUTO_REQUEUE_FAILED_SQL)
        .bind(MAX_FAILED_RETRIES)
        .bind(FAILED_RETRY_COOLDOWN_MINUTES as i32)
        .bind(MAX_ASSIGN_PER_TICK)
        .bind(ff_routing_policy::LOCAL_LANE_MAX_TRIES as i32)
        .execute(pg)
        .await?
        .rows_affected())
}

async fn requeue_deployed_local_retests(pg: &PgPool) -> Result<u64> {
    Ok(sqlx::query(REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL)
        .bind(MAX_ASSIGN_PER_TICK)
        .execute(pg)
        .await?
        .rows_affected())
}

async fn route_diagnosed_local_failures(pg: &PgPool) -> Result<u64> {
    Ok(sqlx::query(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL)
        .bind(MAX_ASSIGN_PER_TICK)
        .execute(pg)
        .await?
        .rows_affected())
}

async fn reconcile_deploy_pending_local_failures(pg: &PgPool) -> Result<u64> {
    Ok(sqlx::query(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL)
        .bind(MAX_ASSIGN_PER_TICK)
        .execute(pg)
        .await?
        .rows_affected())
}

/// One scheduler pass. Returns the number of work_items assigned this tick.
pub async fn evaluate_work_items(pg: &PgPool) -> Result<usize> {
    let stale_secs = lease_stale_secs(pg).await;
    let reaped = ff_db::pg_reap_stale_work_item_leases(
        pg,
        stale_secs,
        MAX_LEASE_DURATION_SECS,
        MAX_BUILD_ATTEMPTS,
    )
    .await?;
    if reaped > 0 {
        warn!(
            reaped,
            "work_item_scheduler: reaped stale leases (slots freed, items re-queued)"
        );
    }

    let routed_diagnoses = route_diagnosed_local_failures(pg).await?;
    if routed_diagnoses > 0 {
        info!(
            routed_diagnoses,
            "work_item_scheduler: routed local-failure diagnoses to improvement pipelines"
        );
    }

    let deployed_remediations = reconcile_deploy_pending_local_failures(pg).await?;
    if deployed_remediations > 0 {
        info!(
            deployed_remediations,
            "work_item_scheduler: deployed local-failure remediations"
        );
    }

    let local_retests = requeue_deployed_local_retests(pg).await?;
    if local_retests > 0 {
        info!(
            local_retests,
            "work_item_scheduler: queued deployed remediations for local-first verification"
        );
    }

    // Companion sweep: `in_progress` work_items with no active lease can't be
    // reaped by the lease sweep above (they have no lease row). Cancel the ones
    // older than ORPHAN_MIN_AGE_SECS so they stop polluting `in_progress`.
    let orphans = ff_db::pg_reap_orphaned_work_items(pg, ORPHAN_MIN_AGE_SECS).await?;
    if orphans > 0 {
        warn!(
            orphans,
            "work_item_scheduler: cancelled orphaned in_progress work_items (no active lease)"
        );
    }

    let retried = auto_requeue_failed_work_items(pg).await?;
    if retried > 0 {
        info!(
            retried,
            cooldown_minutes = FAILED_RETRY_COOLDOWN_MINUTES,
            max_retries = MAX_FAILED_RETRIES,
            "work_item_scheduler: requeued failed work_items at cloud attempt tier"
        );
    }

    // Self-heal: terminally-`failed` items whose last_error was TRANSIENT
    // infrastructure (backend spawn, provider/network, pool, heartbeat) are
    // buildable once the condition clears — return a batch to the ready pool
    // with full redispatch eligibility restored (leases released, assignment
    // cleared). Best-effort: a sweep failure must never stall assignment.
    match crate::self_heal::requeue_transient_failures(pg).await {
        Ok(healed) if healed > 0 => info!(
            healed,
            "work_item_scheduler: self-heal requeued transiently-failed work_items"
        ),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "work_item_scheduler: self-heal requeue sweep failed"),
    }

    // SYSTEMIC-ERROR DOCTOR: before the per-item retry loop wastes the backlog on
    // a shared infrastructure fault (a missing DB column, a migration crash, a
    // dead router), detect clusters of DIFFERENT items failing with the IDENTICAL
    // error, park them (halt the retry storm), and alert the operator with the
    // signature + remediation hint. Best-effort — never stalls scheduling.
    match crate::self_heal::detect_systemic_failures(pg).await {
        Ok(findings) if !findings.is_empty() => {
            for f in &findings {
                warn!(
                    signature = %f.signature, count = f.count, hint = %f.remediation_hint,
                    "work_item_scheduler: SYSTEMIC failure cluster — parked + operator-alert"
                );
                crate::self_heal::alert_systemic_finding(pg, f).await;
            }
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "work_item_scheduler: systemic-error doctor sweep failed"),
    }

    // Auto-complete decomposed parents (bug/feature) once all of their task
    // children are terminal. This stops parent rows from lingering in `ready`
    // and cluttering the board after their leaves finish.
    let completed_parents = ff_db::pg_complete_parent_work_items(pg).await?;
    if completed_parents > 0 {
        info!(
            completed_parents,
            "work_item_scheduler: auto-completed parent work_items"
        );
    }

    // Keep reconciliation and stale-lease cleanup alive during a recovery
    // freeze, but stop before fetching capacity or creating any new leases.
    if !work_item_execution_enabled(pg).await {
        tracing::debug!(
            key = WORK_ITEM_EXECUTION_ENABLED_KEY,
            "work_item_scheduler: new assignments paused by execution gate"
        );
        return Ok(0);
    }

    let ready = ff_db::pg_ready_work_items(pg, MAX_ASSIGN_PER_TICK).await?;
    if ready.is_empty() {
        return Ok(0);
    }

    // Slots that are free fleet-wide (a pinned item filters to its host).
    let mut active_by_computer: HashMap<uuid::Uuid, usize> = sqlx::query(
        "SELECT computer_id, COUNT(*)::bigint AS active \
           FROM work_item_leases \
          WHERE released_at IS NULL \
          GROUP BY computer_id",
    )
    .fetch_all(pg)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.get("computer_id"),
            row.get::<i64, _>("active").max(0) as usize,
        )
    })
    .collect();
    // Active projects fleet-wide (by currently-leased work_items), used below
    // to cap each project's fair share of this tick's slot capacity. Distinct
    // from `interleave_by_project`, which only reorders THIS tick's ready set —
    // a project already holding a disproportionate share of ACTIVE leases must
    // be deprioritized even if its ready backlog looks the same size as a
    // less-active project's. Read straight off `work_item_leases.project_id`
    // (denormalized at lease-assignment time) instead of joining `work_items`.
    let active_by_project: HashMap<Option<String>, usize> =
        ff_db::pg_active_lease_counts_by_project(pg)
            .await?
            .into_iter()
            .map(|(project_id, active)| (project_id, active.max(0) as usize))
            .collect();
    let mut global_free = ff_db::pg_free_slots(pg, None, MAX_ASSIGN_PER_TICK).await?;
    let now = Utc::now();
    let dispatch_live: HashSet<uuid::Uuid> =
        sqlx::query("SELECT id, dispatch_tick_at FROM computers")
            .fetch_all(pg)
            .await?
            .into_iter()
            .filter_map(|row| {
                let id = row.get("id");
                let tick = row.get::<Option<DateTime<Utc>>, _>("dispatch_tick_at");
                dispatch_tick_is_fresh(tick, now).then_some(id)
            })
            .collect();
    global_free.retain(|slot| dispatch_live.contains(&slot.computer_id));
    global_free.retain(|slot| dispatch_capacity_left(&active_by_computer, slot.computer_id));
    if global_free.is_empty() {
        info!(
            ready = ready.len(),
            "work_item_scheduler: items ready but no free slots"
        );
        return Ok(0);
    }

    // Prefer slots on computers with a LIVE agent-capable LLM endpoint so we
    // don't hand a build to a node whose model is already dead at tick time
    // (the E3 finding: a stale 'healthy' row wasted a ~6min lease cycle on
    // priya). This is a PREFERENCE, not a gate — `pop_slot` falls back to any
    // free slot if no viable one remains, so assignment never starves when the
    // deployment rows are momentarily stale (e.g. right after a deploy).
    let viable: HashSet<uuid::Uuid> = match ff_db::pg_agent_viable_computer_ids(pg).await {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            warn!(error = %e, "work_item_scheduler: agent-viability lookup failed; assigning without preference");
            std::collections::HashSet::new()
        }
    };

    let mut pool: Vec<ff_db::FreeSlot> = global_free;
    let mut assigned = 0usize;
    let mut fallback_assigns = 0usize;

    let interleaved = interleave_by_project(ready);
    let distinct_projects: HashSet<&Option<String>> = active_by_project
        .keys()
        .chain(interleaved.iter().map(|i| &i.project_id))
        .collect();
    let total_capacity = active_by_project.values().sum::<usize>() + pool.len();
    let fair_share = project_fair_share(distinct_projects.len(), total_capacity);

    // Two passes so fair-share stays work-conserving: first give every project
    // first refusal up to `fair_share`; anything a project couldn't take because
    // it was already at/over share (`deferred`) gets a second shot once every
    // project has been through pass one, so free slots never sit idle just
    // because the projects that could use them were momentarily capped.
    let mut assigned_this_tick: HashMap<Option<String>, usize> = HashMap::new();
    let mut deferred: Vec<ff_db::ReadyWorkItem> = Vec::new();
    for item in interleaved {
        if project_at_fair_share(
            &item.project_id,
            &active_by_project,
            &assigned_this_tick,
            fair_share,
        ) {
            deferred.push(item);
            continue;
        }
        if try_assign_item(
            pg,
            &item,
            &mut pool,
            &mut active_by_computer,
            &dispatch_live,
            &viable,
            &mut fallback_assigns,
        )
        .await
        {
            assigned += 1;
            *assigned_this_tick
                .entry(item.project_id.clone())
                .or_default() += 1;
        }
    }
    for item in deferred {
        if try_assign_item(
            pg,
            &item,
            &mut pool,
            &mut active_by_computer,
            &dispatch_live,
            &viable,
            &mut fallback_assigns,
        )
        .await
        {
            assigned += 1;
            *assigned_this_tick
                .entry(item.project_id.clone())
                .or_default() += 1;
        }
    }

    if assigned > 0 {
        info!(
            assigned,
            fallback_assigns, "work_item_scheduler: assigned work_items to fleet slots"
        );
    }
    if fallback_assigns > 0 {
        // Not silent: surface that we leased build work to nodes with no live
        // agent endpoint in the DB. Expected transiently after a deploy; if it
        // persists, agent-capability detection or the reconciler is lagging.
        warn!(
            fallback_assigns,
            "work_item_scheduler: assigned to non-agent-viable slots (no live agent endpoint); \
             self-heal will reclaim if the build stalls"
        );
    }
    Ok(assigned)
}

fn dispatch_tick_is_fresh(tick_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    tick_at.is_some_and(|tick| tick >= now - chrono::Duration::seconds(DISPATCH_TICK_STALE_SECS))
}

/// Attempt to assign one ready work_item to a free slot, updating the shared
/// pool / dispatch-capacity bookkeeping on success. Extracted so the fair-share
/// two-pass loop in `evaluate_work_items` (first pass: projects under their
/// share, second pass: work-conserving overflow) shares one assignment path
/// instead of forking it.
#[allow(clippy::too_many_arguments)]
async fn try_assign_item(
    pg: &PgPool,
    item: &ff_db::ReadyWorkItem,
    pool: &mut Vec<ff_db::FreeSlot>,
    active_by_computer: &mut HashMap<uuid::Uuid, usize>,
    dispatch_live: &HashSet<uuid::Uuid>,
    viable: &HashSet<uuid::Uuid>,
    fallback_assigns: &mut usize,
) -> bool {
    // Honor a host pin by re-querying that host's free slots; else take from
    // the shared pool, preferring an agent-viable computer.
    let slot = if let Some(host) = item.assigned_computer.as_deref() {
        match ff_db::pg_free_slots(pg, Some(host), 1).await {
            Ok(mut v) => v
                .pop()
                .filter(|slot| dispatch_live.contains(&slot.computer_id))
                .filter(|slot| dispatch_capacity_left(active_by_computer, slot.computer_id)),
            Err(e) => {
                warn!(host, error = %e, "work_item_scheduler: pinned-slot lookup failed");
                None
            }
        }
    } else {
        pop_slot(pool, viable, fallback_assigns)
    };
    let Some(slot) = slot else { return false };

    match ff_db::pg_assign_work_item(
        pg,
        item.id,
        slot.sub_agent_id,
        slot.computer_id,
        LEASE_GRANT_SECS,
    )
    .await
    {
        Ok(true) => {
            *active_by_computer.entry(slot.computer_id).or_default() += 1;
            let lease_id = sqlx::query_scalar::<_, uuid::Uuid>(
                "SELECT id FROM work_item_leases
                  WHERE work_item_id = $1
                    AND sub_agent_id = $2
                    AND computer_id = $3
                    AND released_at IS NULL
                  ORDER BY created_at DESC
                  LIMIT 1",
            )
            .bind(item.id)
            .bind(slot.sub_agent_id)
            .bind(slot.computer_id)
            .fetch_optional(pg)
            .await
            .ok()
            .flatten();
            if let Some(lease_id) = lease_id {
                spawn_claim_heartbeat(
                    pg.clone(),
                    lease_id,
                    item.id,
                    slot.sub_agent_id,
                    slot.computer_id,
                );
            } else {
                warn!(
                    work_item_id = %item.id,
                    sub_agent_id = %slot.sub_agent_id,
                    "work_item_scheduler: assigned item but could not resolve exact lease for claim heartbeat"
                );
            }
            // Keep the shared pool consistent if a pinned assignment consumed
            // a slot that also sat in `pool`, and remove the rest of this
            // computer's slots as soon as its dispatch capacity is full.
            pool.retain(|s| {
                s.sub_agent_id != slot.sub_agent_id
                    && dispatch_capacity_left(active_by_computer, s.computer_id)
            });
            true
        }
        Ok(false) => false, // lost the race / already leased
        Err(e) => {
            warn!(item = %item.id, error = %e, "work_item_scheduler: assign failed");
            false
        }
    }
}

/// Each project's fair share of this tick's total slot capacity (pre-existing
/// active leases + still-free slots), split evenly across every project that
/// is either currently active or has ready work. Ceil-divided so a remainder
/// favors filling slots over under-assigning. Pure so fair-share sizing is
/// testable without a database.
fn project_fair_share(distinct_projects: usize, total_capacity: usize) -> usize {
    if distinct_projects == 0 {
        return total_capacity;
    }
    total_capacity.div_ceil(distinct_projects)
}

/// True once `project_id` has reached (or exceeded) its fair share of slot
/// capacity, counting both its pre-existing active leases and whatever this
/// tick has already assigned it. The scheduler defers items past this point to
/// a work-conserving second pass instead of dropping them, so a capped project
/// still gets surplus capacity once every project has had first refusal. Pure
/// so the skip rule is testable without a database.
fn project_at_fair_share(
    project_id: &Option<String>,
    active_by_project: &HashMap<Option<String>, usize>,
    assigned_this_tick: &HashMap<Option<String>, usize>,
    fair_share: usize,
) -> bool {
    let active = active_by_project.get(project_id).copied().unwrap_or(0);
    let assigned_now = assigned_this_tick.get(project_id).copied().unwrap_or(0);
    active + assigned_now >= fair_share
}

/// Keep a newly-created lease alive while it waits for the owning host's
/// dispatch loop. Once dispatch changes the item from `claimed` to `building`,
/// `dispatch_one`'s own guard takes over for the rest of the lease lifecycle.
fn spawn_claim_heartbeat(
    pg: PgPool,
    lease_id: uuid::Uuid,
    work_item_id: uuid::Uuid,
    sub_agent_id: uuid::Uuid,
    computer_id: uuid::Uuid,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::work_item_dispatch::HEARTBEAT_SECS,
        ));
        loop {
            ticker.tick().await;
            let still_queued = ff_db::pg_heartbeat_work_item_lease(
                &pg,
                lease_id,
                work_item_id,
                sub_agent_id,
                computer_id,
                "claimed",
                LEASE_GRANT_SECS,
            )
            .await
            .unwrap_or(true);
            if !still_queued {
                break;
            }
        }
    });
}

fn dispatch_capacity_left(
    active_by_computer: &HashMap<uuid::Uuid, usize>,
    computer_id: uuid::Uuid,
) -> bool {
    active_by_computer.get(&computer_id).copied().unwrap_or(0)
        < crate::work_item_dispatch::MAX_DISPATCH_PER_TICK as usize
}

/// Take one free slot from `pool`, preferring a slot whose computer currently
/// has a live agent-capable LLM endpoint (`viable`). Falls back to any free slot
/// (bumping `fallback_assigns`) so assignment never starves when the deployment
/// rows are momentarily stale. Pure so the prefer-with-fallback rule is testable.
fn pop_slot(
    pool: &mut Vec<ff_db::FreeSlot>,
    viable: &std::collections::HashSet<uuid::Uuid>,
    fallback_assigns: &mut usize,
) -> Option<ff_db::FreeSlot> {
    if let Some(idx) = pool.iter().position(|s| viable.contains(&s.computer_id)) {
        return Some(pool.remove(idx));
    }
    // No viable slot left — fall back to any free slot (preserves prior pop()).
    let slot = pool.pop();
    if slot.is_some() {
        *fallback_assigns += 1;
    }
    slot
}

/// Round-robin the ready list across projects, preserving each project's
/// internal (risk/age) order, so assignment order gives every project a fair
/// share of this tick's free slots. `pg_ready_work_items` already ranks
/// per-project BEFORE its LIMIT so no project can monopolize the fetched set;
/// this pass enforces the same guarantee on selection order locally, so the
/// scheduler stays fair even if the fetch ordering regresses. Work-conserving:
/// no item is dropped — once smaller projects drain, surplus slots go to
/// whatever remains. A NULL project_id is its own bucket. Pure so fair-share
/// enforcement is testable without a database.
fn interleave_by_project(items: Vec<ff_db::ReadyWorkItem>) -> Vec<ff_db::ReadyWorkItem> {
    // Vec-of-buckets (not HashMap) keeps project order = first appearance,
    // which the fetch query already sorted by top-item priority.
    let mut buckets: Vec<(Option<String>, VecDeque<ff_db::ReadyWorkItem>)> = Vec::new();
    for item in items {
        match buckets.iter_mut().find(|(p, _)| *p == item.project_id) {
            Some((_, q)) => q.push_back(item),
            None => buckets.push((item.project_id.clone(), VecDeque::from([item]))),
        }
    }
    let total: usize = buckets.iter().map(|(_, q)| q.len()).sum();
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        for (_, q) in buckets.iter_mut() {
            if let Some(item) = q.pop_front() {
                out.push(item);
            }
        }
    }
    out
}

/// Spawn the leader-gated scheduler loop. The skip path reads the process-local
/// leader cache instead of probing Postgres.
pub fn spawn_work_item_scheduler(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(e) = evaluate_work_items(&pg).await {
                        warn!(error = %e, "work_item_scheduler tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work_item_scheduler loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_gate_defaults_and_ttl_restore_are_enabled_but_errors_fail_closed() {
        assert!(WORK_ITEM_EXECUTION_DEFAULT);
        assert!(WORK_ITEM_EXECUTION_RESTORE_ON_EXPIRY);
        assert!(resolve_work_item_execution_gate(Ok::<bool, anyhow::Error>(
            true
        )));
        assert!(!resolve_work_item_execution_gate(
            Ok::<bool, anyhow::Error>(false)
        ));
        assert!(!resolve_work_item_execution_gate(Err(anyhow::anyhow!(
            "gate authority unavailable"
        ))));
    }

    #[test]
    fn execution_gate_runs_after_housekeeping_and_before_assignment() {
        let source = include_str!("work_item_scheduler.rs");
        let body = source
            .split("pub async fn evaluate_work_items")
            .nth(1)
            .expect("scheduler evaluator exists");
        let housekeeping = body
            .find("pg_complete_parent_work_items")
            .expect("final housekeeping step exists");
        let gate = body
            .find("if !work_item_execution_enabled(pg).await")
            .expect("execution gate exists");
        let assignment = body
            .find("pg_ready_work_items")
            .expect("assignment fetch exists");

        assert!(
            housekeeping < gate,
            "gate must preserve scheduler housekeeping"
        );
        assert!(
            gate < assignment,
            "gate must stop before new assignment work"
        );
        assert!(
            body[gate..assignment].contains("return Ok(0)"),
            "disabled gate must return without assigning"
        );
    }

    #[test]
    fn failed_retry_query_is_bounded_atomic_and_excludes_terminal_markers() {
        assert!(AUTO_REQUEUE_FAILED_SQL.contains("w.retry_count < $1"));
        assert!(AUTO_REQUEUE_FAILED_SQL.contains("make_interval(mins => $2)"));
        assert!(AUTO_REQUEUE_FAILED_SQL.contains("FOR UPDATE SKIP LOCKED"));
        assert!(AUTO_REQUEUE_FAILED_SQL.contains("retry_count = w.retry_count + 1"));
        assert!(AUTO_REQUEUE_FAILED_SQL.contains("BOGUS|QUARANTINE|CANCELLED"));
        assert!(AUTO_REQUEUE_FAILED_SQL.contains("l.released_at IS NULL"));
        assert_eq!(MAX_FAILED_RETRIES, 3);
        assert_eq!(FAILED_RETRY_COOLDOWN_MINUTES, 20);
    }

    #[test]
    fn deployed_local_retest_query_is_atomic_local_first_and_idempotent() {
        assert!(REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL.contains("remediation_status = 'deployed'"));
        assert!(REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL.contains("deployed_at IS NOT NULL"));
        assert!(REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL.contains("FOR UPDATE OF d2 SKIP LOCKED"));
        assert!(
            REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL
                .contains("remediation_status = 'local_retest_running'")
        );
        assert!(REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL.contains("attempts = 0"));
        assert!(REQUEUE_DEPLOYED_LOCAL_RETESTS_SQL.contains("l.released_at IS NULL"));
    }

    #[test]
    fn diagnosed_local_failures_route_to_existing_improvement_pipelines() {
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("local_context_sources"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("local_context_chunks"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("UPDATE work_items w"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("'local_failure_improvement'"));
        assert!(
            ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("EXISTS (SELECT 1 FROM context_updates)")
        );
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("training_jobs"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("dreamer_context_pack"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("fine_tune_model_ab"));
        assert!(
            ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("remediation_status = 'deploy_pending'")
        );
    }

    #[test]
    fn diagnosed_local_failure_routing_is_idempotent_by_diagnosis_id() {
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("d.remediation_status = 'diagnosed'"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("FOR UPDATE SKIP LOCKED"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("local-failure-diagnosis://' || c.id"));
        assert!(ROUTE_DIAGNOSED_LOCAL_FAILURES_SQL.contains("'local_failure_diagnosis_id', c.id"));
    }

    #[test]
    fn deploy_reconciliation_requires_an_available_local_improvement() {
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL
                .contains("d.remediation_status = 'deploy_pending'")
        );
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL
                .contains("JOIN local_context_chunks c ON c.source_id = s.id")
        );
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("t.status = 'completed'"));
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL
                .contains("NULLIF(t.result_model_id, '') IS NOT NULL")
        );
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("NOT EXISTS ("));
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("AND EXISTS ("));
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("target.status = 'online'"));
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("target.has_gpu"));
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL
                .contains("target.reservation_state <> 'drained'")
        );
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("f.worker_name = target.name")
        );
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("c.computer_id = target.id"));
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("f.health_status = 'healthy'")
        );
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("f.desired_state = 'active'"));
        assert!(RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("c.status = 'active'"));
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL.contains("FOR UPDATE OF d SKIP LOCKED")
        );
        assert!(
            RECONCILE_DEPLOY_PENDING_LOCAL_FAILURES_SQL
                .contains("deployed_at = COALESCE(d.deployed_at, NOW())")
        );
    }

    #[tokio::test]
    async fn context_gap_remediation_routes_deploys_and_requeues_same_item_locally() {
        let url = match std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        {
            Ok(url) => url,
            Err(_) => {
                eprintln!(
                    "skipping cloud-fixes-local DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
                );
                return;
            }
        };
        let pool = match sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
        {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping cloud-fixes-local DB test: database unavailable: {e}");
                return;
            }
        };
        ff_db::run_postgres_migrations(&pool)
            .await
            .expect("migrations should create cloud-fixes-local tables");

        let work_item_id = uuid::Uuid::new_v4();
        let diagnosis_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO work_items (id, project_id, kind, title, status, attempts)
             VALUES ($1, 'forge-fleet', 'task', 'cloud-fixes-local acceptance', 'done', 3)",
        )
        .bind(work_item_id)
        .execute(&pool)
        .await
        .expect("insert acceptance work item");
        sqlx::query(
            "INSERT INTO local_failure_diagnoses
                (id, work_item_id, rescue_attempt, local_failure_summary, cloud_backend,
                 cloud_diagnosis, cause_class, improvement_route)
             VALUES ($1, $2, 4, 'local edited the wrong module', 'codex',
                     'include the owning dispatch module in the context pack',
                     'context_prompt_gap', 'dreamer_context_pack')",
        )
        .bind(diagnosis_id)
        .bind(work_item_id)
        .execute(&pool)
        .await
        .expect("insert local-failure diagnosis");

        assert_eq!(
            route_diagnosed_local_failures(&pool)
                .await
                .expect("route diagnosis"),
            1
        );
        let routed: (String, i64, serde_json::Value) = sqlx::query_as(
            "SELECT d.remediation_status,
                    (SELECT COUNT(*)
                       FROM local_context_sources s
                       JOIN local_context_chunks c ON c.source_id = s.id
                      WHERE s.uri = 'local-failure-diagnosis://' || d.id),
                    w.context
               FROM local_failure_diagnoses d
               JOIN work_items w ON w.id = d.work_item_id
              WHERE d.id = $1",
        )
        .bind(diagnosis_id)
        .fetch_one(&pool)
        .await
        .expect("read routed diagnosis");
        assert_eq!(routed.0, "deploy_pending");
        assert_eq!(routed.1, 1);
        assert_eq!(
            routed.2["local_failure_improvement"]["diagnosis_id"],
            diagnosis_id.to_string()
        );
        assert_eq!(
            route_diagnosed_local_failures(&pool)
                .await
                .expect("repeat route"),
            0,
            "a scheduler retry must not duplicate the context artifact"
        );

        assert_eq!(
            reconcile_deploy_pending_local_failures(&pool)
                .await
                .expect("deploy shared context"),
            1
        );
        assert_eq!(
            requeue_deployed_local_retests(&pool)
                .await
                .expect("requeue original item"),
            1
        );
        let requeued: (String, i32, String) = sqlx::query_as(
            "SELECT w.status, w.attempts, d.remediation_status
               FROM work_items w
               JOIN local_failure_diagnoses d ON d.work_item_id = w.id
              WHERE w.id = $1",
        )
        .bind(work_item_id)
        .fetch_one(&pool)
        .await
        .expect("read local-first requeue");
        assert_eq!(requeued, ("ready".into(), 0, "local_retest_running".into()));
        assert_eq!(
            requeue_deployed_local_retests(&pool)
                .await
                .expect("repeat requeue"),
            0
        );

        sqlx::query(
            "DELETE FROM local_context_sources
              WHERE uri = 'local-failure-diagnosis://' || $1::uuid",
        )
        .bind(diagnosis_id)
        .execute(&pool)
        .await
        .expect("delete acceptance context source");
        sqlx::query("DELETE FROM local_failure_diagnoses WHERE id = $1")
            .bind(diagnosis_id)
            .execute(&pool)
            .await
            .expect("delete acceptance diagnosis");
        sqlx::query("DELETE FROM work_items WHERE id = $1")
            .bind(work_item_id)
            .execute(&pool)
            .await
            .expect("delete acceptance work item");
    }

    fn slot(computer: uuid::Uuid) -> ff_db::FreeSlot {
        ff_db::FreeSlot {
            sub_agent_id: uuid::Uuid::new_v4(),
            computer_id: computer,
        }
    }

    fn ready(project: Option<&str>) -> ff_db::ReadyWorkItem {
        ff_db::ReadyWorkItem {
            id: uuid::Uuid::new_v4(),
            assigned_computer: None,
            project_id: project.map(str::to_owned),
        }
    }

    /// Fair-share enforcement: even a worst-case fetched set where one project's
    /// items arrive as a contiguous block ahead of everyone else's (the attempt-1
    /// monopoly failure) must be reordered so every ready project appears within
    /// the first `distinct_projects` picks. While every project still has items
    /// queued, no project may hold more than `k` of the first
    /// `k * distinct_projects` picks.
    #[test]
    fn fair_share_stops_one_project_monopolizing_selection() {
        let mut items: Vec<_> = (0..6).map(|_| ready(Some("alpha"))).collect();
        items.extend([
            ready(Some("beta")),
            ready(Some("beta")),
            ready(Some("gamma")),
        ]);
        let out = interleave_by_project(items);

        let projects_in_prefix: HashSet<_> =
            out[..3].iter().map(|i| i.project_id.clone()).collect();
        assert_eq!(
            projects_in_prefix.len(),
            3,
            "first 3 picks must cover all 3 ready projects"
        );

        // Equal backlogs (3 projects x 3 items, alpha's block first): the k-cap
        // invariant holds for every round because no bucket drains early.
        let mut even: Vec<_> = (0..3).map(|_| ready(Some("alpha"))).collect();
        even.extend((0..3).map(|_| ready(Some("beta"))));
        even.extend((0..3).map(|_| ready(Some("gamma"))));
        let out = interleave_by_project(even);
        for k in 1..=3 {
            for project in ["alpha", "beta", "gamma"] {
                let share = out[..k * 3]
                    .iter()
                    .filter(|i| i.project_id.as_deref() == Some(project))
                    .count();
                assert_eq!(
                    share,
                    k,
                    "{project} took {share} of the first {} picks (fair share is {k})",
                    k * 3
                );
            }
        }
    }

    /// Work-conserving: interleaving reorders but never drops items — once the
    /// smaller projects drain, the surplus project fills the remaining picks,
    /// and each project's internal (risk/age) order is preserved.
    #[test]
    fn fair_share_is_work_conserving_and_order_stable() {
        let alpha: Vec<_> = (0..4).map(|_| ready(Some("alpha"))).collect();
        let beta = vec![ready(Some("beta"))];
        let alpha_ids: Vec<_> = alpha.iter().map(|i| i.id).collect();
        let mut items = alpha;
        items.extend(beta);
        let out = interleave_by_project(items);

        assert_eq!(out.len(), 5, "no item may be dropped");
        assert!(
            out[2..]
                .iter()
                .all(|i| i.project_id.as_deref() == Some("alpha")),
            "surplus picks must fall to the remaining project, not go unused"
        );
        let alpha_out: Vec<_> = out
            .iter()
            .filter(|i| i.project_id.as_deref() == Some("alpha"))
            .map(|i| i.id)
            .collect();
        assert_eq!(
            alpha_out, alpha_ids,
            "within-project order must be preserved"
        );
    }

    /// Items with no project_id form their own fair-share bucket rather than
    /// being merged into another project or starved.
    #[test]
    fn fair_share_treats_null_project_as_own_bucket() {
        let items = vec![
            ready(Some("alpha")),
            ready(Some("alpha")),
            ready(None),
            ready(None),
        ];
        let out = interleave_by_project(items);
        assert!(
            out[..2].iter().any(|i| i.project_id.is_none()),
            "project-less items must get a fair-share pick too"
        );
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn dispatch_capacity_counts_active_leases() {
        let computer = uuid::Uuid::new_v4();
        let mut active = HashMap::new();
        assert!(dispatch_capacity_left(&active, computer));
        active.insert(
            computer,
            crate::work_item_dispatch::MAX_DISPATCH_PER_TICK as usize,
        );
        assert!(!dispatch_capacity_left(&active, computer));
    }

    /// E3 finding: prefer an agent-viable computer's slot when one exists, so a
    /// build isn't handed to a node with no live LLM endpoint.
    #[test]
    fn pop_slot_prefers_a_viable_computer() {
        let dead = uuid::Uuid::new_v4();
        let live = uuid::Uuid::new_v4();
        // dead node's slot is "fresher" (would win a plain pop()) but has no LLM.
        let mut pool = vec![slot(live), slot(dead)];
        let viable: std::collections::HashSet<_> = [live].into_iter().collect();
        let mut fb = 0;
        let picked = pop_slot(&mut pool, &viable, &mut fb).unwrap();
        assert_eq!(picked.computer_id, live, "must pick the live-endpoint node");
        assert_eq!(fb, 0, "a viable pick is not a fallback");
        assert_eq!(pool.len(), 1, "exactly one slot consumed");
    }

    /// Safety: when NO slot is agent-viable (e.g. rows stale right after a
    /// deploy), assignment must still proceed via fallback rather than starve.
    #[test]
    fn pop_slot_falls_back_when_none_viable() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut pool = vec![slot(a), slot(b)];
        let viable = std::collections::HashSet::new();
        let mut fb = 0;
        assert!(pop_slot(&mut pool, &viable, &mut fb).is_some());
        assert_eq!(fb, 1, "fallback assignment must be counted, not silent");
        // Empty pool yields None without bumping the fallback counter.
        let mut empty: Vec<ff_db::FreeSlot> = vec![];
        assert!(pop_slot(&mut empty, &viable, &mut fb).is_none());
        assert_eq!(fb, 1);
    }

    /// REGRESSION GUARD (reaper bug class #589/#590): same coupling as
    /// `lease_takeover` — the scheduler's own lease-reap window must clear at
    /// least two dispatch heartbeats so a live build's lease is never reclaimed.
    #[test]
    fn lease_stale_window_floor_clears_two_heartbeats() {
        let cadence = crate::work_item_dispatch::HEARTBEAT_SECS as i64;
        assert!(
            MIN_LEASE_STALE_SECS >= 2 * cadence,
            "MIN_LEASE_STALE_SECS ({MIN_LEASE_STALE_SECS}) must be >= 2x the dispatch heartbeat ({cadence})"
        );
    }

    #[test]
    fn lease_stale_window_uses_successful_build_p99_with_margin() {
        assert_eq!(
            lease_stale_secs_from_success_p99(135, Some(517.57952862)),
            647
        );
    }

    #[test]
    fn lease_stale_window_falls_back_without_enough_samples() {
        assert_eq!(
            lease_stale_secs_from_success_p99(LEASE_STALE_MIN_SAMPLES - 1, Some(1357.0)),
            MIN_LEASE_STALE_SECS
        );
        assert_eq!(
            lease_stale_secs_from_success_p99(LEASE_STALE_MIN_SAMPLES, None),
            MIN_LEASE_STALE_SECS
        );
    }

    #[test]
    fn lease_stale_window_clamps_extreme_measurements() {
        assert_eq!(
            lease_stale_secs_from_success_p99(LEASE_STALE_MIN_SAMPLES, Some(60.0)),
            MIN_LEASE_STALE_SECS
        );
        assert_eq!(
            lease_stale_secs_from_success_p99(LEASE_STALE_MIN_SAMPLES, Some(10_000.0)),
            MAX_LEASE_STALE_SECS
        );
    }

    #[test]
    fn lease_stale_samples_bootstrap_until_real_build_durations_are_populated() {
        assert_eq!(
            select_lease_stale_sample(LEASE_STALE_MIN_SAMPLES - 1, Some(600.0), 135, Some(517.0)),
            ("released_at-dispatch_tick_at-bootstrap", 135, Some(517.0))
        );
        assert_eq!(
            select_lease_stale_sample(LEASE_STALE_MIN_SAMPLES, Some(600.0), 135, Some(517.0)),
            (
                "released_at-build_started_at",
                LEASE_STALE_MIN_SAMPLES,
                Some(600.0)
            )
        );
        assert_eq!(
            select_lease_stale_sample(LEASE_STALE_MIN_SAMPLES, None, 135, Some(517.0)),
            ("released_at-dispatch_tick_at-bootstrap", 135, Some(517.0))
        );
    }

    #[test]
    fn lease_stale_samples_are_valid_recent_successful_completions() {
        assert!(LEASE_STALE_SAMPLE_SQL.contains("release_reason = 'ready for review'"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("released_at - build_started_at"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("build_started_at IS NOT NULL"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("released_at > build_started_at"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("released_at - dispatch_tick_at"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("build_started_at IS NULL"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("dispatch_tick_at IS NOT NULL"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("released_at > dispatch_tick_at"));
        assert!(!LEASE_STALE_SAMPLE_SQL.contains("created_at"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("released_at <= NOW()"));
        assert!(LEASE_STALE_SAMPLE_SQL.contains("released_at >= NOW() - make_interval"));
    }

    /// Capacity splits evenly (ceil-divided) across every distinct project so a
    /// remainder favors filling slots over under-assigning.
    #[test]
    fn project_fair_share_splits_capacity_evenly() {
        assert_eq!(project_fair_share(3, 9), 3);
        assert_eq!(project_fair_share(3, 10), 4, "remainder rounds up");
        assert_eq!(
            project_fair_share(0, 5),
            5,
            "no projects: share is the whole pool"
        );
    }

    /// A project with no pre-existing active leases and nothing assigned yet
    /// this tick is under its share; once its active-plus-this-tick count
    /// reaches the share it must be skipped (deferred), not assigned further.
    #[test]
    fn project_at_fair_share_counts_active_and_this_tick_assignments() {
        let alpha = Some("alpha".to_string());
        let mut active = HashMap::new();
        active.insert(alpha.clone(), 2usize);
        let mut assigned_this_tick = HashMap::new();

        assert!(
            !project_at_fair_share(&alpha, &active, &assigned_this_tick, 3),
            "2 active < share of 3"
        );

        assigned_this_tick.insert(alpha.clone(), 1);
        assert!(
            project_at_fair_share(&alpha, &active, &assigned_this_tick, 3),
            "2 active + 1 this tick reaches the share of 3"
        );

        let beta = Some("beta".to_string());
        assert!(
            !project_at_fair_share(&beta, &active, &assigned_this_tick, 3),
            "an untracked project has 0 active and 0 assigned so it is under share"
        );
    }

    #[test]
    fn stale_dispatch_tick_is_not_assignment_eligible() {
        let now = Utc::now();
        assert!(!dispatch_tick_is_fresh(None, now));
        assert!(dispatch_tick_is_fresh(
            Some(now - chrono::Duration::seconds(DISPATCH_TICK_STALE_SECS)),
            now
        ));
        assert!(!dispatch_tick_is_fresh(
            Some(now - chrono::Duration::seconds(DISPATCH_TICK_STALE_SECS + 1)),
            now
        ));
    }
}
