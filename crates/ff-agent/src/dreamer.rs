//! The memory **dreamer** — ForgeFleet's sleep-time consolidation loop.
//!
//! Background counterpart to the event-driven scratchpad consolidation in
//! [`crate::scratchpad`]: that path only fires when a *write* pushes a scope
//! over its byte cap (the MemGPT "flush at 100%" analog), so memory that stops
//! being written — above all **session scopes whose session has ended** — is
//! never consolidated and lingers forever. The dreamer is the Letta/MemGPT
//! "sleep-time agent" analog: a recurring background pass that curates memory
//! while no agent is actively using it.
//!
//! ## What one pass does
//! 1. **Archive dead session scopes** (Letta sleep-time + MemGPT eviction):
//!    every `session`-scope scratchpad untouched for
//!    [`SESSION_SCOPE_IDLE_SECS`] is pushed into Brain as knowledge candidates
//!    (full text, nothing lost), recorded in the eviction audit trail, and
//!    deleted. Episodic working memory graduates to long-term storage instead
//!    of rotting in place — the Graphiti episodic→semantic promotion pattern.
//! 2. **Re-enforce byte caps** on the durable `agent`/`project` scopes:
//!    normally a no-op (the write path already enforces caps), but catches
//!    scopes left over-cap by a lowered cap or a crashed consolidation.
//!
//! ## Cadence — a self-rescheduling chain on `deferred_tasks`
//! Each pass runs as ONE deferred task (`kind='internal'`,
//! title [`DREAMER_TASK_TITLE`]) and, on completion, enqueues the next link
//! with an `at_time` trigger [`DREAMER_INTERVAL_SECS`] ahead — recurrence via
//! the existing one-shot queue, no new scheduler. The chain is self-healing:
//! [`schedule_next_run`] refuses to enqueue when another active dreamer task
//! already exists, so accidental duplicate chains converge back to one, and
//! [`ensure_dreamer_scheduled`] (called at forgefleetd startup) re-seeds a
//! chain that died (e.g. the queue was purged).
//!
//! The 30-min due-check with work-thresholds inside (idle-TTL, bounded batch)
//! follows the cross-system cadence pattern (Letta sleeptime fires every ~5
//! agent steps; generative agents reflect on an accumulated-importance
//! threshold; Zep pairs cheap per-event upkeep with periodic full passes):
//! check often, work only when thresholds are met. A pass that finds nothing
//! stale costs two SELECTs. Design + tuning: `plans/memory-consolidation-cadence.md`.

use anyhow::{Context, Result};
use sqlx::PgPool;
use tracing::{info, warn};

/// Deferred-task title the defer worker dispatches on (`kind='internal'`).
pub const DREAMER_TASK_TITLE: &str = "memory-dreamer";

/// Default seconds between dreamer passes (30 min). Override via
/// `fleet_secrets.dreamer_interval_secs`; clamped to [`MIN_INTERVAL_SECS`].
pub const DREAMER_INTERVAL_SECS: i64 = 1800;

/// Floor for the configured interval — a shorter chain would just burn
/// scheduler passes re-checking thresholds that move on multi-hour scales.
pub const MIN_INTERVAL_SECS: i64 = 300;

/// A `session`-scope scratchpad untouched this long is dead: agent sessions
/// run minutes-to-hours, so 6h of silence means the session ended and nothing
/// will read the scope again. (The write path can't catch these — they go
/// stale precisely because writes stopped.)
pub const SESSION_SCOPE_IDLE_SECS: i64 = 6 * 3600;

/// Max session scopes archived per pass — bounds one pass's Brain-insert and
/// delete work; the 30-min chain drains any backlog across passes.
pub const MAX_SESSION_SWEEPS_PER_PASS: i64 = 16;
/// Max compaction episodes admitted to semantic-memory candidates per pass.
pub const MAX_EPISODE_INTAKE_PER_PASS: i64 = 16;

/// fleet_secrets gate: `off`/`false`/`0`/`disabled`/`no` skips the pass body
/// (the chain keeps ticking so flipping the gate back on needs no re-seed).
const DREAMER_MODE_KEY: &str = "dreamer_mode";
/// fleet_secrets override for the seconds between passes.
const DREAMER_INTERVAL_KEY: &str = "dreamer_interval_secs";

/// `off`/`false`/`0`/`disabled`/`no` (case-insensitive) disable the pass body;
/// any other value — including a missing secret — leaves it ON.
fn mode_is_off(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("off" | "false" | "0" | "disabled" | "no")
    )
}

/// Effective inter-pass interval: the parsed override when valid, else the
/// default; never below [`MIN_INTERVAL_SECS`].
fn effective_interval_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(DREAMER_INTERVAL_SECS)
        .max(MIN_INTERVAL_SECS)
}

async fn read_secret(pool: &PgPool, key: &str) -> Option<String> {
    match ff_db::pg_get_secret(pool, key).await {
        Ok(v) => v,
        Err(e) => {
            warn!(key, error = %e, "dreamer: failed to read secret; using default");
            None
        }
    }
}

/// Does an active (pending / dispatchable / running) dreamer task other than
/// `exclude_id` already exist? Guards against duplicate chains.
async fn active_chain_exists(pool: &PgPool, exclude_id: Option<&str>) -> Result<bool> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM fleet_tasks
             WHERE task_class = 'deferred'
               AND summary = $1
               AND status IN ('pending', 'dispatchable', 'running')
               AND ($2::text IS NULL OR id::text <> $2)
         )",
    )
    .bind(DREAMER_TASK_TITLE)
    .bind(exclude_id)
    .fetch_one(pool)
    .await
    .context("probe for an active dreamer task")?;
    Ok(exists)
}

/// Enqueue the next chain link with an `at_time` trigger one interval ahead,
/// unless another active dreamer task (excluding `current_task_id`, the link
/// that is finishing) already exists. Returns whether a link was enqueued.
///
/// `max_attempts = 1`: a failed pass must NOT retry-loop — the next link is
/// already scheduled and re-covers the same work (every step is idempotent).
pub async fn schedule_next_run(pool: &PgPool, current_task_id: Option<&str>) -> Result<bool> {
    if active_chain_exists(pool, current_task_id).await? {
        return Ok(false);
    }
    let interval =
        effective_interval_secs(read_secret(pool, DREAMER_INTERVAL_KEY).await.as_deref());
    let at = chrono::Utc::now() + chrono::Duration::seconds(interval);
    ff_db::queries::pg_enqueue_deferred(
        pool,
        DREAMER_TASK_TITLE,
        "internal",
        &serde_json::json!({}),
        "at_time",
        &serde_json::json!({ "at": at.to_rfc3339() }),
        None,
        &serde_json::json!([]),
        Some("dreamer"),
        Some(1),
    )
    .await
    .context("enqueue next dreamer link")?;
    Ok(true)
}

/// Seed the dreamer chain if no active link exists — idempotent, called at
/// forgefleetd startup on every node (the EXISTS probe keeps it single).
pub async fn ensure_dreamer_scheduled(pool: &PgPool) -> Result<bool> {
    schedule_next_run(pool, None).await
}

/// One dreamer pass. Returns a JSON report stored as the deferred task's
/// result. Every step is idempotent and bounded, so a crashed or failed pass
/// is simply re-covered by the next chain link.
pub async fn run_dreamer_pass(pool: &PgPool) -> Result<serde_json::Value> {
    if mode_is_off(read_secret(pool, DREAMER_MODE_KEY).await.as_deref()) {
        return Ok(serde_json::json!({ "skipped": "dreamer_mode=off" }));
    }

    // 1) Archive session scopes whose session is long over.
    let stale: Vec<String> = sqlx::query_scalar(
        "SELECT scope_key FROM agent_memory
          WHERE scope_type = 'session'
          GROUP BY scope_key
         HAVING MAX(updated_at) < NOW() - make_interval(secs => $1)
          ORDER BY MAX(updated_at)
          LIMIT $2",
    )
    .bind(SESSION_SCOPE_IDLE_SECS as f64)
    .bind(MAX_SESSION_SWEEPS_PER_PASS)
    .fetch_all(pool)
    .await
    .context("list stale session scopes")?;

    let mut sessions_archived = 0usize;
    let mut blocks_archived = 0usize;
    for scope_key in &stale {
        match crate::scratchpad::archive_session_scope(pool, scope_key).await {
            Ok(n) => {
                sessions_archived += 1;
                blocks_archived += n;
            }
            Err(e) => {
                warn!(scope_key, error = %e, "dreamer: session archive failed; will retry next pass")
            }
        }
    }

    // 2) Admit compacted-context episodes to semantic-memory intake.
    let episodes_intaken =
        intake_compaction_episodes(pool).await? + intake_fleet_episodes(pool).await?;

    // 3) Re-enforce caps on durable scopes (no-op unless a cap was lowered or
    //    a previous consolidation crashed mid-way).
    let durable: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT scope_type, scope_key FROM agent_memory
          WHERE scope_type IN ('agent', 'project')",
    )
    .fetch_all(pool)
    .await
    .context("list durable memory scopes")?;

    let mut scopes_consolidated = 0usize;
    for (scope_type, scope_key) in &durable {
        let cap = ff_db::queries::pg_memory_cap(pool, scope_type, scope_key).await?;
        let total = ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
        if total <= cap as i64 {
            continue;
        }
        match crate::scratchpad::consolidate_and_forget(pool, scope_type, scope_key, cap).await {
            Ok(true) => scopes_consolidated += 1,
            Ok(false) => {}
            Err(e) => {
                warn!(scope_type, scope_key, error = %e, "dreamer: cap re-enforcement failed")
            }
        }
    }

    if sessions_archived > 0 || episodes_intaken > 0 || scopes_consolidated > 0 {
        info!(
            sessions_archived,
            blocks_archived,
            episodes_intaken,
            scopes_consolidated,
            "dreamer: consolidation pass did work"
        );
    }
    Ok(serde_json::json!({
        "sessions_archived": sessions_archived,
        "blocks_archived": blocks_archived,
        "episodes_intaken": episodes_intaken,
        "over_cap_scopes_consolidated": scopes_consolidated,
    }))
}

async fn intake_compaction_episodes(pool: &PgPool) -> Result<u64> {
    let user = match ff_db::pg_get_brain_user(pool, "venkat").await? {
        Some(user) => user.id,
        None => ff_db::pg_create_brain_user(pool, "venkat", Some("Venkat")).await?,
    };
    let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
        "INSERT INTO brain_knowledge_candidates
            (user_id, action, kind, title, body, tags, project, target_path,
             from_thread, confidence)
         SELECT $1, 'create', 'episode', n.title, n.body, n.tags, n.project,
                n.path, n.from_thread, 1.0
           FROM brain_vault_nodes n
          WHERE n.node_type = 'episode' AND n.valid_until IS NULL
            AND n.body IS NOT NULL
            AND NOT EXISTS (
                SELECT 1 FROM brain_knowledge_candidates c
                 WHERE c.kind = 'episode' AND c.target_path = n.path
            )
          ORDER BY n.valid_from
          LIMIT $2
         RETURNING id",
    )
    .bind(user)
    .bind(MAX_EPISODE_INTAKE_PER_PASS)
    .fetch_all(pool)
    .await
    .context("admit compaction episodes to dreamer intake")?;
    Ok(rows.len() as u64)
}

async fn intake_fleet_episodes(pool: &PgPool) -> Result<u64> {
    let user = match ff_db::pg_get_brain_user(pool, "venkat").await? {
        Some(user) => user.id,
        None => ff_db::pg_create_brain_user(pool, "venkat", Some("Venkat")).await?,
    };
    let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
        "INSERT INTO brain_knowledge_candidates
            (user_id, action, kind, title, body, tags, project, target_path,
             from_thread, confidence)
         SELECT $1, 'create', 'episode',
                e.source_kind || ' episode on ' || e.node,
                e.content,
                ARRAY[e.source_kind, 'node:' || e.node, 'role:' || e.role],
                NULL,
                'episode://fleet/' || e.id::text,
                e.session_id,
                1.0
           FROM fleet_episodes e
          WHERE e.content <> ''
            AND NOT EXISTS (
                SELECT 1 FROM brain_knowledge_candidates c
                 WHERE c.kind = 'episode'
                   AND c.target_path = 'episode://fleet/' || e.id::text
            )
          ORDER BY e.ts
          LIMIT $2
         RETURNING id",
    )
    .bind(user)
    .bind(MAX_EPISODE_INTAKE_PER_PASS)
    .fetch_all(pool)
    .await
    .context("admit fleet episodes to dreamer intake")?;
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("auto"), Some("1800")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }

    #[test]
    fn interval_override_parses_and_clamps() {
        assert_eq!(effective_interval_secs(None), DREAMER_INTERVAL_SECS);
        assert_eq!(effective_interval_secs(Some("3600")), 3600);
        assert_eq!(effective_interval_secs(Some(" 900 ")), 900);
        // Garbage falls back to the default; tiny values clamp to the floor.
        assert_eq!(effective_interval_secs(Some("soon")), DREAMER_INTERVAL_SECS);
        assert_eq!(effective_interval_secs(Some("10")), MIN_INTERVAL_SECS);
        assert_eq!(effective_interval_secs(Some("-5")), MIN_INTERVAL_SECS);
    }

    /// The chain must survive a pass whose body errors: scheduling the next
    /// link is decoupled from pass success (see defer_worker's dreamer branch,
    /// which calls `schedule_next_run` regardless of the pass result) and a
    /// failed link never retries (max_attempts = 1 at enqueue).
    #[test]
    fn chain_constants_are_sane() {
        assert!(DREAMER_INTERVAL_SECS >= MIN_INTERVAL_SECS);
        assert!(SESSION_SCOPE_IDLE_SECS > DREAMER_INTERVAL_SECS);
        assert!(MAX_SESSION_SWEEPS_PER_PASS > 0);
        assert!(MAX_EPISODE_INTAKE_PER_PASS > 0);
    }
}
