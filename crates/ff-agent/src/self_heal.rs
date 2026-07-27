//! Self-heal requeue for work_items that terminally failed on TRANSIENT errors.
//!
//! The dispatch retry ladder (`work_item_dispatch::requeue_or_fail`) marks a
//! work_item terminal `failed` once its attempt budget is exhausted — even when
//! every attempt died on an INFRASTRUCTURE failure (backend spawn, heartbeat
//! takeover, DB pool, provider/network, host-resource exhaustion) rather than
//! anything wrong with the task itself. Those items are buildable once the
//! infra condition clears (creds fixed, node back online, rate-limit window
//! passed), so this sweep returns them to the ready pool for another try.
//!
//! Requeue restores FULL redispatch eligibility, mirroring what
//! `pg_reap_stale_work_item_leases` undoes on takeover: `pg_ready_work_items`
//! skips any item with an unreleased lease, and `pg_assign_work_item` can't
//! insert a second active lease past the partial-unique index — so flipping
//! `status` alone is NOT enough. Each requeue also releases active leases,
//! clears `assigned_to`/`assigned_computer`, fails live worktree rows, and
//! frees any slot still pointing at the item.

use anyhow::Result;
use sqlx::PgPool;
use tracing::info;

/// Error signatures marking a stored `last_error` as a TRANSIENT infrastructure
/// failure (vs a task-level failure — compile error, test failure, lint — that
/// retrying without a code change cannot fix). Shared with the dispatch retry
/// prompt: `work_item_dispatch::retry_error_is_actionable` treats exactly this
/// class as not-actionable. Signatures are consolidated from live dispatch
/// errors + an `ff council` (codex+kimi) pass; kept deliberately unambiguous so
/// a real Rust compile/test error is never matched.
pub const TRANSIENT_ERROR_SIGNATURES: &[&str] = &[
    // dispatch / backend spawn + routing
    "no dispatchable backend",
    "all backends failed on this node",
    "spawn \"",
    "command timed out",
    "timed out after",
    // heartbeat / lease lifecycle
    "stale-heartbeat",
    "heartbeat takeover",
    // datastore / pool
    "pool timed out",
    "pool timeout",
    "route deployments",
    // auth / provider / network (LLM endpoint or gh)
    "gh auth login",
    "bad credentials",
    "rate limit",
    "service unavailable",
    "internal server error",
    "connection refused",
    "network is unreachable",
    // host resource exhaustion
    "no space left",
    "cannot allocate memory",
    "too many open files",
    "resource temporarily unavailable",
    "worker died",
    // build-OUTCOME transient (operator 2026-07-25: "why isn't ff working on the
    // failed items?"). These are retryable NOW that dispatch is local-first: a
    // different/local backend, a healthy reviewer, or a cleared stall/lock lets
    // the SAME task land on a later attempt. Bounded by MAX_SELF_HEAL_RETRIES so
    // a genuinely-impossible task still stops after its retries.
    "in-place review unavailable", // reviewer (480b/cloud) was down — retry when back
    "produced no diff",            // backend made no change — local-first may succeed
    "stalled attempts",            // stall/lock class — retry after the reaper clears it
    "git reset",                   // dirty worktree — a fresh worktree clears it
    "could not fetch origin",      // transient git/network fetch failure
    // worktree-setup transient (operator 2026-07-26): a per-node worktree/cache
    // hiccup ("create repo parent", "clone project repo", "clone from cache")
    // is node-local — a retry lands the item on a healthy node (or the node
    // recovers). These stranded 7 real items in `failed` with attempts=4 until
    // manually reset; now ff self-heals them.
    "create repo parent",
    "clone project repo",
    "clone from cache",
];

/// Whether a stored `last_error` matches a [`TRANSIENT_ERROR_SIGNATURES`]
/// infrastructure signature (case-insensitive substring match).
pub fn error_is_transient(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    TRANSIENT_ERROR_SIGNATURES
        .iter()
        .any(|sig| lower.contains(sig))
}

/// Failed-item retry budget. This is deliberately the same cap as the general
/// scheduler retry path; transient classification must not create an
/// independent, unbounded retry budget.
pub const MAX_SELF_HEAL_RETRIES: i32 = 3;

/// Max items returned to the ready pool per sweep (back-pressure: a fleet-wide
/// outage can terminally fail a large backlog at once; re-admit it gradually).
pub const SELF_HEAL_REQUEUE_BATCH: i64 = 16;

/// Skip items whose last lease released within this window. The transient
/// condition that failed them (dead creds, offline node, rate limit) rarely
/// clears in seconds, and the scheduler ticks every ~15s — without a cooldown
/// an item would burn its whole self-heal budget inside a single outage.
pub const SELF_HEAL_COOLDOWN_SECS: i64 = 20 * 60;

// ---------------------------------------------------------------------------
// SYSTEMIC-ERROR DOCTOR (operator 2026-07-26): ff self-heals BUILD failures but
// not INFRASTRUCTURE failures. When many DIFFERENT work_items fail with the SAME
// error (a missing DB column, a migration crash, "no healthy fleet deployment"),
// that is ONE systemic fault — not N task bugs — and the per-item retry loop
// CANNOT fix it (the building agent can't fix a schema/migration/router problem),
// so it silently grinds the whole backlog to terminal failure over ~an hour. The
// doctor detects the cluster fast, HALTS the retry storm (parks the items so they
// stop burning retries), and ALERTS the operator with the exact signature + a
// remediation hint — turning "operator catches it via screenshot an hour later"
// into "ff catches it at failure #3 and surfaces the fix".
// ---------------------------------------------------------------------------

/// A detected systemic failure cluster.
#[derive(Debug, Clone)]
pub struct SystemicFinding {
    pub signature: String,
    pub count: i64,
    pub sample_error: String,
    pub remediation_hint: String,
}

/// Minimum distinct failed items sharing one normalized signature to call it
/// systemic (not a per-task coincidence). 3 different tasks with the IDENTICAL
/// error = a shared root cause.
pub const SYSTEMIC_CLUSTER_THRESHOLD: i64 = 3;

/// Normalize a `last_error` into a stable clustering key: lowercase, strip the
/// VARIABLE parts (uuids, hex, long digit runs, `[attempt N]` tags, tail-trimmed
/// stderr) while KEEPING the error STRUCTURE + identifiers (a column/table name).
/// So the 32 "column \"build_started_at\" does not exist" failures collapse to
/// ONE key, but varied per-task errors ("review rejected: <different reason>")
/// stay distinct and are NOT clustered/parked. PURE (testable).
pub fn normalize_error_signature(err: &str) -> String {
    let mut s = err.to_ascii_lowercase();
    // Drop a leading "[attempt N] " tag.
    if let Some(rest) = s.strip_prefix('[')
        && let Some(idx) = rest.find(']')
    {
        s = rest[idx + 1..].trim_start().to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            // Collapse any run of digits to a single '#'.
            while chars.peek().is_some_and(|n| n.is_ascii_digit()) {
                chars.next();
            }
            out.push('#');
        } else {
            out.push(c);
        }
    }
    // Collapse whitespace and cap length so the tail (huge command echoes /
    // stderr dumps) doesn't fragment otherwise-identical signatures.
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

/// A clustered signature that is TASK-LEVEL (each item hard for the model) — the
/// retry ladder escalates these; the doctor must NOT park them.
fn is_task_level_cluster(sig: &str) -> bool {
    const TASK_LEVEL: &[&str] = &[
        "stalled attempts",
        "produced no diff",
        "review rejected",
        "self-verify failed",
        "diff is empty",
        "no diff",
    ];
    TASK_LEVEL.iter().any(|s| sig.contains(s))
}

/// A clustered signature that IS a genuine infrastructure fault the per-item
/// retry loop can never fix — the doctor parks + alerts on exactly these.
fn is_infra_fault(sig: &str) -> bool {
    const INFRA: &[&str] = &[
        "does not exist", // missing DB column/table/relation
        "column",         // schema mismatch
        "relation",
        "migration",                   // migration crash
        "no healthy fleet deployment", // dead router
        "no dispatchable backend",
        "could not fetch", // git access broken
        "refusing to build",
        "syntax error", // bad SQL/migration
        "connection refused",
        "pool timed out",
    ];
    INFRA.iter().any(|s| sig.contains(s))
}

/// A remediation hint for a known systemic signature (best-effort operator guidance).
fn remediation_hint_for(sig: &str) -> String {
    if sig.contains("does not exist") && sig.contains("column") {
        "a DB column is missing — likely a migration didn't apply. Check `_migrations` vs schema.rs; \
         apply the missing `ADD COLUMN IF NOT EXISTS` and requeue.".to_string()
    } else if sig.contains("migration") {
        "a Postgres migration is failing on daemon startup (crash-loop risk). Fix/skip the migration, \
         then reset-failed + restart forgefleetd fleet-wide.".to_string()
    } else if sig.contains("no healthy fleet deployment") || sig.contains("no dispatchable backend")
    {
        "the LLM router has no serving model — check deployment health (`fleet_model_deployments` \
         fresh?) and daemon liveness."
            .to_string()
    } else if sig.contains("could not fetch") || sig.contains("clone") {
        "git fetch/clone is failing on a node — check the PAT/SSH remote and network to GitHub."
            .to_string()
    } else if sig.contains("produced no diff") {
        "the routed LLM keeps producing no diff — the model may be wedged; check the router rotation.".to_string()
    } else {
        "recurring identical failure across many items — a shared infrastructure fault; investigate the signature.".to_string()
    }
}

/// Detect systemic failure clusters, HALT the retry storm (park them), and record
/// findings for the operator. Returns the findings so the caller can alert.
/// Leader-gated by the caller. Parks only clusters ≥ threshold — genuinely
/// per-task failures (each a distinct error) never cluster, so they're untouched.
pub async fn detect_systemic_failures(pg: &PgPool) -> Result<Vec<SystemicFinding>> {
    // Pull recent failed items with an error, cluster in Rust by normalized sig.
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, last_error FROM work_items \
          WHERE status = 'failed' AND last_error IS NOT NULL AND last_error <> '' \
            AND NOT parked",
    )
    .fetch_all(pg)
    .await?;

    let mut clusters: std::collections::HashMap<String, (i64, String, Vec<uuid::Uuid>)> =
        std::collections::HashMap::new();
    for (id, err) in rows {
        let sig = normalize_error_signature(&err);
        let e = clusters
            .entry(sig)
            .or_insert_with(|| (0, err.clone(), Vec::new()));
        e.0 += 1;
        e.2.push(id);
    }

    let mut findings = Vec::new();
    for (sig, (count, sample, ids)) in clusters {
        if count < SYSTEMIC_CLUSTER_THRESHOLD {
            continue;
        }
        // NOT everything that clusters is a systemic INFRA fault the operator must
        // fix. A cluster of "stalled attempts" / "produced no diff" / "review
        // rejected" is N tasks each too hard for the routed model — the retry
        // ladder (escalate to cloud, rotate LLM) OWNS those; parking them would
        // HALT progress instead of escalating. Only PARK+ALERT clusters whose
        // signature is a genuine infra fault the per-item loop can NEVER fix
        // (missing DB column, migration crash, dead router, git-fetch). Skip a
        // cluster that's already a known transient/build-retry signature.
        if error_is_transient(&sample) || is_task_level_cluster(&sig) {
            continue;
        }
        if !is_infra_fault(&sig) {
            continue;
        }
        // Halt the storm: park every item in this cluster so the scheduler stops
        // re-dispatching them into the same wall (they cannot self-fix a systemic
        // fault). A human/auto-remediation un-parks them once the root cause is fixed.
        let parked =
            sqlx::query("UPDATE work_items SET parked = true WHERE id = ANY($1) AND NOT parked")
                .bind(&ids)
                .execute(pg)
                .await
                .map(|r| r.rows_affected())
                .unwrap_or(0);
        let hint = remediation_hint_for(&sig);
        tracing::warn!(
            signature = %sig,
            count,
            parked,
            hint = %hint,
            sample = %sample.chars().take(120).collect::<String>(),
            "SYSTEMIC-ERROR DOCTOR: cluster of identical failures detected — PARKED to halt retry storm; operator remediation needed"
        );
        findings.push(SystemicFinding {
            signature: sig,
            count,
            sample_error: sample.chars().take(200).collect(),
            remediation_hint: hint,
        });
    }
    Ok(findings)
}

/// Alert the operator about a systemic failure cluster — deduped to at most once
/// per signature per hour (via `operator_notify_dedup`) so a persistent fault
/// doesn't spam. Sends to Telegram if configured; always logs. Best-effort.
pub async fn alert_systemic_finding(pg: &PgPool, f: &SystemicFinding) {
    // Hourly single-flight per signature (same table/pattern as task-fail alerts).
    let should_send: bool = sqlx::query_scalar(
        "INSERT INTO operator_notify_dedup (signature, last_sent) VALUES ($1, NOW()) \
         ON CONFLICT (signature) DO UPDATE SET last_sent = NOW() \
           WHERE operator_notify_dedup.last_sent < NOW() - INTERVAL '1 hour' \
         RETURNING true",
    )
    .bind(format!("systemic:{}", f.signature))
    .fetch_optional(pg)
    .await
    .ok()
    .flatten()
    .unwrap_or(false);
    if !should_send {
        return; // throttled — already alerted this signature within the hour
    }
    let title = "🩺 ForgeFleet doctor: systemic failure";
    let body = format!(
        "{} items are failing with the SAME error — a shared infrastructure fault, \
         not per-task bugs. Parked to stop the retry storm.\n\n\
         Signature: {}\n\nSample: {}\n\n→ {}",
        f.count, f.signature, f.sample_error, f.remediation_hint
    );
    if let Err(e) = crate::telegram::send_telegram_from_secrets(pg, title, &body).await {
        tracing::warn!(error = %e, signature = %f.signature, "alert_systemic_finding: telegram send failed (logged only)");
    }
}

/// One self-heal sweep with the default knobs; called from the scheduler tick.
pub async fn requeue_transient_failures(pg: &PgPool) -> Result<u64> {
    requeue_transient_failures_with(
        pg,
        MAX_SELF_HEAL_RETRIES,
        SELF_HEAL_REQUEUE_BATCH,
        SELF_HEAL_COOLDOWN_SECS,
    )
    .await
}

/// Requeue up to `batch` terminally-`failed` task work_items whose `last_error`
/// is transient (see [`TRANSIENT_ERROR_SIGNATURES`]) and whose `attempts` is
/// still under `max_retries`, restoring full redispatch eligibility in one
/// transaction-equivalent statement. Returns the number of items requeued.
pub async fn requeue_transient_failures_with(
    pg: &PgPool,
    max_retries: i32,
    batch: i64,
    cooldown_secs: i64,
) -> Result<u64> {
    // `%sig%` patterns for `LIKE ANY` — signatures contain no LIKE wildcards.
    let patterns: Vec<String> = TRANSIENT_ERROR_SIGNATURES
        .iter()
        .map(|sig| format!("%{sig}%"))
        .collect();

    let rows = sqlx::query_scalar::<_, uuid::Uuid>(
        "WITH candidates AS (
             SELECT w.id
               FROM work_items w
              WHERE w.status = 'failed'
                AND w.kind = 'task'
                AND w.retry_count < $1
                -- Transient classification MUST sit here, BEFORE the LIMIT: a
                -- LIMIT over all failed items with classification applied
                -- afterwards lets a page of older non-transient failures starve
                -- every transient one behind it, forever.
                AND lower(COALESCE(w.last_error, '')) LIKE ANY($2)
                AND w.completed_at <= NOW() - make_interval(secs => $4)
                AND COALESCE(w.last_error, '') !~*
                    '(^|[^A-Z])(BOGUS|QUARANTINE|CANCELLED)([^A-Z]|$)'
              ORDER BY w.created_at ASC
              LIMIT $3
                FOR UPDATE SKIP LOCKED
         ), released_leases AS (
             UPDATE work_item_leases l
                SET lease_state = 'released',
                    released_at = NOW(),
                    release_reason = 'self-heal transient requeue'
               FROM candidates c
              WHERE l.work_item_id = c.id
                AND l.released_at IS NULL
         ), freed_slots AS (
             UPDATE sub_agents sa
                SET current_work_item_id = NULL,
                    status = 'idle'
               FROM candidates c
              WHERE sa.current_work_item_id = c.id
         ), failed_worktrees AS (
             UPDATE work_item_worktrees t
                SET status = 'failed'
               FROM candidates c
              WHERE t.work_item_id = c.id
                AND t.status IN ('creating', 'active')
         )
         UPDATE work_items w
            SET status = 'ready',
                retry_count = w.retry_count + 1,
                attempts = GREATEST(
                    COALESCE(w.attempts, 0),
                    $5
                ),
                assigned_to = NULL,
                assigned_computer = NULL,
                completed_at = NULL
           FROM candidates c
          WHERE w.id = c.id
      RETURNING w.id",
    )
    .bind(max_retries)
    .bind(&patterns)
    .bind(batch)
    .bind(cooldown_secs as f64)
    .bind(ff_routing_policy::LOCAL_LANE_MAX_TRIES as i32)
    .fetch_all(pg)
    .await?;

    if !rows.is_empty() {
        info!(
            requeued = rows.len(),
            max_retries, batch, "self-heal: requeued transiently-failed work_items"
        );
    }
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::env;

    #[test]
    fn transient_classification_matrix() {
        for transient in [
            "connect: Connection refused (os error 111)",
            "codex: 429 Too Many Requests — rate limit exceeded",
            "stale-heartbeat takeover (attempt 3)",
            "pool timed out while waiting for an open connection",
            "No space left on device (os error 28)",
            // build-outcome transient (retryable with local-first)
            "in-place review unavailable: no in-place review backend available",
            "backend kimi produced no diff (no commits) — required change not applied",
            "failed after 7 stalled attempts (max 5 reached)",
            "command failed: git reset --hard",
            "checkout_clone_for_build: could not fetch origin/main",
        ] {
            assert!(error_is_transient(transient), "{transient:?}");
        }
        for task_level in [
            "error[E0308]: mismatched types",
            "test result: FAILED. 1 passed; 2 failed",
            "cargo fmt --check found diffs",
        ] {
            assert!(!error_is_transient(task_level), "{task_level:?}");
        }
    }

    // -- DB tests: early-return (skip) when no Postgres is configured; CI's
    //    `cargo test --lib` has no database and must never panic here.

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_self_heal_requeue_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn create_temp_db() -> Option<(PgPool, PgPool, String)> {
        let (admin_url, db_url, db_name) = temp_db_urls()?;
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        // Minimal slice of the live schema: only the tables + columns the
        // requeue statement touches (no cross-table FKs needed for the test).
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE work_items (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 kind TEXT NOT NULL DEFAULT 'task',
                 status TEXT NOT NULL,
                 attempts INT NOT NULL DEFAULT 0,
                 last_error TEXT,
                 assigned_to TEXT,
                 assigned_computer TEXT,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             );
             CREATE TABLE work_item_leases (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id UUID NOT NULL,
                 lease_state TEXT NOT NULL DEFAULT 'claimed',
                 released_at TIMESTAMPTZ,
                 release_reason TEXT
             );
             CREATE TABLE work_item_worktrees (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id UUID NOT NULL,
                 status TEXT NOT NULL DEFAULT 'active'
             );
             CREATE TABLE sub_agents (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 current_work_item_id UUID,
                 status TEXT
             );",
        )
        .execute(&pool)
        .await
        .expect("create minimal work_item schema");
        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .ok();
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .ok();
        admin.close().await;
    }

    async fn insert_failed_item(
        pool: &PgPool,
        last_error: &str,
        attempts: i32,
        created_offset_secs: i64,
    ) -> uuid::Uuid {
        sqlx::query_scalar(
            "INSERT INTO work_items
                 (kind, status, attempts, last_error, assigned_to, assigned_computer,
                  created_at, completed_at)
             VALUES ('task', 'failed', $1, $2, 'slot-1', 'computer-1',
                     NOW() - make_interval(secs => $3),
                     NOW() - make_interval(secs => $3))
          RETURNING id",
        )
        .bind(attempts)
        .bind(last_error)
        .bind(created_offset_secs as f64)
        .fetch_one(pool)
        .await
        .expect("insert failed work_item")
    }

    #[tokio::test]
    async fn requeue_clears_assignment_lease_worktree_and_slot() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };

        let transient =
            insert_failed_item(&pool, "dispatch: connection refused by endpoint", 5, 3600).await;
        let task_level = insert_failed_item(&pool, "error[E0308]: mismatched types", 5, 3600).await;
        let exhausted = insert_failed_item(&pool, "rate limit exceeded", 5, 3600).await;
        sqlx::query("UPDATE work_items SET retry_count = $2 WHERE id = $1")
            .bind(exhausted)
            .bind(MAX_SELF_HEAL_RETRIES)
            .execute(&pool)
            .await
            .unwrap();
        let cooling = insert_failed_item(&pool, "service unavailable", 5, 3600).await;

        // Live residue on the transient item: an unreleased lease (blocks both
        // pg_ready_work_items and a new pg_assign_work_item lease), an active
        // worktree, and a slot still pointing at it.
        sqlx::query("INSERT INTO work_item_leases (work_item_id) VALUES ($1)")
            .bind(transient)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO work_item_worktrees (work_item_id, status) VALUES ($1, 'active')")
            .bind(transient)
            .execute(&pool)
            .await
            .unwrap();
        let slot: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO sub_agents (current_work_item_id, status)
             VALUES ($1, 'busy') RETURNING id",
        )
        .bind(transient)
        .fetch_one(&pool)
        .await
        .unwrap();
        // A recent failure timestamp puts `cooling` inside the cooldown window.
        sqlx::query(
            "UPDATE work_items SET completed_at = NOW() - INTERVAL '10 seconds' WHERE id = $1",
        )
        .bind(cooling)
        .execute(&pool)
        .await
        .unwrap();

        let requeued = requeue_transient_failures(&pool).await.expect("requeue");
        assert_eq!(requeued, 1);

        let row = sqlx::query(
            "SELECT status, attempts, retry_count, assigned_to, assigned_computer
               FROM work_items WHERE id = $1",
        )
        .bind(transient)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("status"), "ready");
        assert_eq!(row.get::<i32, _>("attempts"), 5);
        assert_eq!(row.get::<i32, _>("retry_count"), 1);
        assert_eq!(row.get::<Option<String>, _>("assigned_to"), None);
        assert_eq!(row.get::<Option<String>, _>("assigned_computer"), None);

        let lease = sqlx::query(
            "SELECT lease_state, released_at, release_reason
               FROM work_item_leases WHERE work_item_id = $1",
        )
        .bind(transient)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(lease.get::<String, _>("lease_state"), "released");
        assert!(
            lease
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("released_at")
                .is_some()
        );
        assert_eq!(
            lease.get::<Option<String>, _>("release_reason").as_deref(),
            Some("self-heal transient requeue")
        );

        let worktree_status: String =
            sqlx::query_scalar("SELECT status FROM work_item_worktrees WHERE work_item_id = $1")
                .bind(transient)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(worktree_status, "failed");

        let slot_row =
            sqlx::query("SELECT current_work_item_id, status FROM sub_agents WHERE id = $1")
                .bind(slot)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            slot_row.get::<Option<uuid::Uuid>, _>("current_work_item_id"),
            None
        );
        assert_eq!(
            slot_row.get::<Option<String>, _>("status").as_deref(),
            Some("idle")
        );

        // Task-level, attempt-exhausted, and cooling-down items stay failed.
        for (id, why) in [
            (task_level, "task-level error"),
            (exhausted, "retry count at ceiling"),
            (cooling, "inside cooldown window"),
        ] {
            let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(status, "failed", "{why} must not requeue");
        }

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn transient_requeue_not_starved_by_older_nontransient_failures() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };

        // Regression (retry attempt 2): a batch LIMIT applied before transient
        // classification returned only the OLDEST failed items — all
        // non-transient here — and the newer transient failure never requeued.
        let batch = 3i64;
        for i in 0..(batch + 2) {
            insert_failed_item(
                &pool,
                "error[E0308]: mismatched types",
                5,
                86_400 + i * 60, // older than the transient item below
            )
            .await;
        }
        let transient = insert_failed_item(&pool, "network is unreachable", 5, 3600).await;

        let requeued = requeue_transient_failures_with(&pool, MAX_SELF_HEAL_RETRIES, batch, 600)
            .await
            .expect("requeue");
        assert_eq!(requeued, 1);

        let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = $1")
            .bind(transient)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "ready");

        drop_temp_db(admin, pool, &db_name).await;
    }
}

#[cfg(test)]
mod systemic_tests {
    use super::*;

    #[test]
    fn identical_infra_errors_cluster_varied_task_errors_do_not() {
        // The 32 missing-column failures → ONE key (identifier kept, digits→#).
        let a = normalize_error_signature(
            "error returned from database: column \"build_started_at\" does not exist",
        );
        let b = normalize_error_signature(
            "[attempt 3] error returned from database: column \"build_started_at\" does not exist",
        );
        assert_eq!(a, b, "same infra error must collapse to one cluster key");

        // Different missing columns → DIFFERENT keys (distinct problems).
        let c = normalize_error_signature(
            "error returned from database: column \"foo\" does not exist",
        );
        assert_ne!(a, c);

        // Varied per-task review rejections → DIFFERENT keys (stay per-task, not parked).
        let r1 = normalize_error_signature(
            "in-place review rejected by codex: Dropping both legacy tables",
        );
        let r2 = normalize_error_signature(
            "in-place review rejected by codex: The config uses placeholders",
        );
        assert_ne!(r1, r2, "distinct per-task errors must NOT cluster");
        // Classifier: infra faults are parked; task-level clusters are NOT.
        assert!(is_infra_fault(&normalize_error_signature(
            "column \"build_started_at\" does not exist"
        )));
        assert!(is_infra_fault(&normalize_error_signature(
            "no healthy fleet deployment"
        )));
        assert!(is_task_level_cluster(&normalize_error_signature(
            "failed after 5 stalled attempts (max 5 reached)"
        )));
        assert!(is_task_level_cluster(&normalize_error_signature(
            "in-place review rejected by codex: The diff is empty"
        )));
        assert!(!is_infra_fault(&normalize_error_signature(
            "failed after 5 stalled attempts"
        )));
    }
}
