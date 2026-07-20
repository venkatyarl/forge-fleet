//! Self-heal coordination helpers for the leader tick.
//!
//! These functions live outside `leader_tick.rs` so they can be unit-tested
//! without spinning the whole leader state machine.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// Default cooldown before a processed self-heal signature is eligible for
/// re-arming. Prevents thrashing on a bug that flaps every few minutes.
pub const DEFAULT_REARM_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// In-memory state kept by the leader tick for the self-heal subsystem.
///
/// Tracks bug signatures that have been observed so that a previously
/// resolved bug can be recognised when it reappears.
#[derive(Debug, Default, Clone)]
pub struct SelfHealState {
    /// Bug signatures currently tracked by the leader.
    pub tracked_signatures: HashSet<String>,
}

impl SelfHealState {
    /// Create an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start tracking a bug signature.
    ///
    /// Returns `true` if the signature was newly added, or `false` if it was
    /// already tracked.
    pub fn track(&mut self, signature: &str) -> bool {
        self.tracked_signatures.insert(signature.to_owned())
    }

    /// Returns `true` if the signature is currently tracked.
    pub fn is_tracked(&self, signature: &str) -> bool {
        self.tracked_signatures.contains(signature)
    }

    /// Returns `true` when a tracked signature is reported again with status
    /// `detected`, indicating a previously resolved bug has reappeared.
    pub fn is_reappearing(&self, signature: &str, current_status: &str) -> bool {
        self.is_tracked(signature) && current_status == "detected"
    }
}

/// Map a self-heal tier to a `fleet_tasks` priority.
pub fn self_heal_priority_for_tier(tier: &str) -> i32 {
    match tier {
        "T1" => 100,
        "T0" => 90,
        "T2" => 80,
        _ => 70,
    }
}

/// Map a self-heal queue status to a `fleet_tasks` status.
pub fn self_heal_task_status(queue_status: &str) -> &'static str {
    match queue_status {
        "detected" => "pending",
        "fixing" | "reviewing" | "pr_open" | "merged" | "rolled_out" => "running",
        "verified" => "completed",
        "paused" => "paused",
        "reverted" => "cancelled",
        _ => "failed",
    }
}

/// Outcome of checking whether a bug signature should re-arm a self-heal task.
#[derive(Debug, Clone)]
pub struct RearmCheck {
    /// True when the existing task is in a terminal state and has cooled down.
    pub should_rearm: bool,
    /// ID of the existing self-heal task, if one exists.
    pub existing_task_id: Option<uuid::Uuid>,
    /// The current `fleet_tasks.status` of the existing task.
    pub terminal_status: Option<String>,
    /// When the existing task was completed (if recorded).
    pub completed_at: Option<DateTime<Utc>>,
}

/// Check whether a bug signature has already been processed to a terminal
/// state (`completed`, `failed`, or `cancelled`) and has passed its re-arm
/// cooldown.
///
/// When this returns `should_rearm = true`, callers should reset the existing
/// self-heal task back to `pending`/`detected` so that recurring failures are
/// not permanently suppressed by the unique `dedup_signature` constraint.
pub async fn signature_should_rearm(
    pg: &PgPool,
    bug_signature: &str,
    cooldown: Option<std::time::Duration>,
) -> Result<RearmCheck, sqlx::Error> {
    let cooldown = cooldown.unwrap_or(DEFAULT_REARM_COOLDOWN);
    let row = sqlx::query(
        "SELECT id,
                status,
                completed_at,
                created_at
           FROM fleet_tasks
          WHERE task_class = 'self_heal'
            AND dedup_signature = $1",
    )
    .bind(bug_signature)
    .fetch_optional(pg)
    .await?;

    let Some(row) = row else {
        return Ok(RearmCheck {
            should_rearm: false,
            existing_task_id: None,
            terminal_status: None,
            completed_at: None,
        });
    };

    let id: uuid::Uuid = row.try_get("id")?;
    let status: String = row.try_get("status")?;
    let completed_at: Option<DateTime<Utc>> = row.try_get("completed_at")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;

    let terminal = matches!(status.as_str(), "completed" | "failed" | "cancelled");
    let terminal_at = completed_at.unwrap_or(created_at);
    let cooldown_secs = i64::try_from(cooldown.as_secs()).unwrap_or(i64::MAX);
    let cutoff = Utc::now() - chrono::Duration::seconds(cooldown_secs);
    let cooled_down = terminal_at <= cutoff;

    Ok(RearmCheck {
        should_rearm: terminal && cooled_down,
        existing_task_id: Some(id),
        terminal_status: Some(status),
        completed_at,
    })
}

/// Re-arm an existing self-heal task if it has reached a terminal state and
/// has cooled down.
///
/// Returns `true` when the row was actually updated. This is the companion to
/// [`signature_should_rearm`] and is used by [`scan_interaction_errors`] so
/// that recurring interaction-log errors are not discarded by the
/// `ON CONFLICT ... DO NOTHING` insert path.
pub async fn rearm_self_heal_task(
    pg: &PgPool,
    bug_signature: &str,
    tier: &str,
    report_count: i32,
    cooldown: Option<std::time::Duration>,
) -> Result<bool, sqlx::Error> {
    let check = signature_should_rearm(pg, bug_signature, cooldown).await?;
    if !check.should_rearm {
        return Ok(false);
    }

    let terminal_statuses = ["completed", "failed", "cancelled"];
    let updated = sqlx::query(
        "UPDATE fleet_tasks
            SET status = $4,
                priority = $3,
                completed_at = NULL,
                created_at = NOW(),
                payload = payload || jsonb_build_object(
                    'status', 'detected',
                    'attempts', 0,
                    'report_count', COALESCE((payload->>'report_count')::int, 0) + $2,
                    'tier', $5,
                    'rearmed_at', NOW()::text,
                    'last_attempt_at', NULL,
                    'writer_computer_id', NULL,
                    'escalated_to_operator_at', NULL
                )
          WHERE task_class = 'self_heal'
            AND dedup_signature = $1
            AND status = ANY($6)",
    )
    .bind(bug_signature)
    .bind(report_count)
    .bind(self_heal_priority_for_tier(tier))
    .bind(self_heal_task_status("detected"))
    .bind(tier)
    .bind(&terminal_statuses[..])
    .execute(pg)
    .await?;

    Ok(updated.rows_affected() > 0)
}

/// V122+: scan local daemon logs for recurring error/warning patterns and
/// feed them into the self-heal queue.
///
/// A marker file gates the scan to roughly one pass per
/// `FF_SELF_HEAL_LOG_INTERVAL_SECS` (default 5 min) so the leader tick does
/// not re-read logs every 15 s.
///
/// Single-flight is enforced by the unique `dedup_signature` index on
/// `fleet_tasks` (`task_class = 'self_heal'`). Recurring signatures that have
/// reached a terminal state and cooled down are re-armed via
/// [`rearm_self_heal_task`] so log errors do not disappear after the first
/// fix attempt.
///
/// Returns the number of newly enqueued or re-armed signatures.
pub async fn scan_daemon_logs_for_self_heal(
    pg: &PgPool,
    my_name: &str,
) -> Result<u32, sqlx::Error> {
    let config = DaemonLogScanConfig::from_env();
    scan_daemon_logs_with_config(pg, my_name, &config).await
}

async fn scan_daemon_logs_with_config(
    pg: &PgPool,
    my_name: &str,
    config: &DaemonLogScanConfig,
) -> Result<u32, sqlx::Error> {
    if !config.is_enabled() {
        return Ok(0);
    }

    // Marker-file gate (skipped when interval is 0, e.g. tests).
    if config.interval_secs > 0 {
        let home = std::env::var("HOME").unwrap_or_default();
        let marker = format!("{home}/.forgefleet/self-heal-logs.last");
        if let Ok(meta) = std::fs::metadata(&marker)
            && let Ok(modified) = meta.modified()
            && let Ok(elapsed) = modified.elapsed()
            && elapsed.as_secs() < config.interval_secs
        {
            return Ok(0);
        }
    }

    let mut grouped: HashMap<String, RecurringLogPattern> = HashMap::new();

    for path_pattern in &config.paths {
        for path in crate::log_analysis_worker::expand_path_pattern(path_pattern) {
            let lines = read_log_lines(&path, config.tail_lines).await;
            for line in lines {
                if let Some(normalized) =
                    crate::log_analysis_worker::normalize_log_line(&line, &config.patterns)
                {
                    let signature = format!(
                        "log:{}",
                        crate::log_analysis_worker::compute_signature(&normalized)
                    );
                    grouped
                        .entry(signature.clone())
                        .and_modify(|p| {
                            p.count += 1;
                            if p.example.len() < line.len() {
                                p.example = line.clone();
                            }
                        })
                        .or_insert_with(|| RecurringLogPattern {
                            signature,
                            example: line,
                            count: 1,
                            source: path.clone(),
                        });
                }
            }
        }
    }

    let recurring: Vec<&RecurringLogPattern> = grouped
        .values()
        .filter(|p| p.count >= config.min_recurrence)
        .collect();

    let mut enqueued = 0u32;
    for pattern in recurring {
        let inserted = sqlx::query(
            "INSERT INTO fleet_tasks \
                (id, task_type, summary, payload, priority, status, created_at, task_class, dedup_signature) \
             VALUES ( \
                gen_random_uuid(), \
                'self_heal_writer', \
                format('self_heal_writer: %s', $1), \
                jsonb_build_object( \
                    'bug_signature', $1, \
                    'tier', 'T2', \
                    'status', 'detected', \
                    'report_count', $2, \
                    'attempts', 0, \
                    'log_example', $3, \
                    'log_source', $4 \
                ), \
                80, \
                'pending', \
                NOW(), \
                'self_heal', \
                $1 \
             ) \
             ON CONFLICT (dedup_signature) WHERE dedup_signature IS NOT NULL DO NOTHING",
        )
        .bind(&pattern.signature)
        .bind(pattern.count as i32)
        .bind(&pattern.example)
        .bind(pattern.source.display().to_string())
        .execute(pg)
        .await?;

        if inserted.rows_affected() > 0 {
            enqueued += 1;
            tracing::info!(
                node = %my_name,
                error_signature = %pattern.signature,
                report_count = pattern.count,
                "scan_daemon_logs_for_self_heal: enqueued novel log error signature for self-heal"
            );
        } else {
            match rearm_self_heal_task(pg, &pattern.signature, "T2", pattern.count as i32, None)
                .await
            {
                Ok(true) => {
                    enqueued += 1;
                    tracing::info!(
                        node = %my_name,
                        error_signature = %pattern.signature,
                        report_count = pattern.count,
                        "scan_daemon_logs_for_self_heal: re-armed recurring log error signature for self-heal"
                    );
                }
                Ok(false) => {
                    tracing::debug!(
                        error_signature = %pattern.signature,
                        "scan_daemon_logs_for_self_heal: signature already in self-heal queue; skipping"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        node = %my_name,
                        error_signature = %pattern.signature,
                        error = %err,
                        "scan_daemon_logs_for_self_heal: failed to re-arm self-heal signature"
                    );
                }
            }
        }
    }

    // Bump marker.
    if config.interval_secs > 0 {
        let home = std::env::var("HOME").unwrap_or_default();
        let marker = format!("{home}/.forgefleet/self-heal-logs.last");
        if let Some(parent) = std::path::Path::new(&marker).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, Utc::now().to_rfc3339());
    }

    Ok(enqueued)
}

async fn read_log_lines(path: &Path, tail_lines: usize) -> Vec<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => {
            let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            let total = lines.len();
            if total <= tail_lines {
                lines
            } else {
                lines.into_iter().skip(total - tail_lines).collect()
            }
        }
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "scan_daemon_logs_for_self_heal: failed to read log file"
            );
            Vec::new()
        }
    }
}

#[derive(Debug, Clone)]
struct DaemonLogScanConfig {
    paths: Vec<String>,
    patterns: Vec<String>,
    min_recurrence: usize,
    tail_lines: usize,
    interval_secs: u64,
}

impl DaemonLogScanConfig {
    fn from_env() -> Self {
        let paths = parse_csv_env("FF_SELF_HEAL_LOG_PATHS", &["~/.forgefleet/logs/**/*.log"]);
        let patterns = parse_csv_env(
            "FF_SELF_HEAL_LOG_PATTERNS",
            &["ERROR", "FATAL", "EXCEPTION", "WARN"],
        )
        .into_iter()
        .map(|p| p.to_ascii_uppercase())
        .collect();

        let min_recurrence = std::env::var("FF_SELF_HEAL_LOG_MIN_RECURRENCE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);

        let tail_lines = std::env::var("FF_SELF_HEAL_LOG_TAIL_LINES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1000);

        let interval_secs = std::env::var("FF_SELF_HEAL_LOG_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5 * 60);

        Self {
            paths,
            patterns,
            min_recurrence,
            tail_lines,
            interval_secs,
        }
    }

    fn is_enabled(&self) -> bool {
        !self.paths.is_empty() && !self.patterns.is_empty()
    }
}

#[derive(Debug, Clone)]
struct RecurringLogPattern {
    signature: String,
    example: String,
    count: usize,
    source: PathBuf,
}

fn parse_csv_env(name: &str, defaults: &[&str]) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_else(|| defaults.iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::io::Write;

    fn temp_db_urls() -> (String, String, String) {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .expect("FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL must be set for DB tests");
        let (prefix, _) = base_url
            .rsplit_once('/')
            .expect("database URL must end with /<db>");
        let db_name = format!("ff_self_heal_{}", uuid::Uuid::new_v4().simple());
        (
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        )
    }

    async fn create_temp_db() -> (sqlx::PgPool, sqlx::PgPool, String) {
        let (admin_url, db_url, db_name) = temp_db_urls();
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
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE fleet_tasks (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 task_type TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 payload JSONB NOT NULL DEFAULT '{}'::jsonb,
                 priority INT NOT NULL DEFAULT 50,
                 status TEXT NOT NULL DEFAULT 'pending',
                 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                 completed_at TIMESTAMPTZ,
                 task_class TEXT,
                 dedup_signature TEXT
             );
             CREATE UNIQUE INDEX idx_fleet_tasks_dedup_signature
                 ON fleet_tasks (dedup_signature)
                 WHERE dedup_signature IS NOT NULL;",
        )
        .execute(&pool)
        .await
        .expect("create minimal fleet_tasks schema");
        (admin, pool, db_name)
    }

    async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
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
        .expect("terminate temp db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }

    async fn insert_self_heal_task(
        pg: &sqlx::PgPool,
        bug_signature: &str,
        status: &str,
        completed_at: Option<DateTime<Utc>>,
    ) -> uuid::Uuid {
        let row = sqlx::query(
            "INSERT INTO fleet_tasks
                (id, task_type, summary, payload, priority, status, created_at, completed_at, task_class, dedup_signature)
             VALUES (
                gen_random_uuid(),
                'self_heal_writer',
                $1,
                jsonb_build_object('bug_signature', $1, 'status', $2),
                80,
                $2,
                NOW(),
                $3,
                'self_heal',
                $1
             )
             RETURNING id",
        )
        .bind(bug_signature)
        .bind(status)
        .bind(completed_at)
        .fetch_one(pg)
        .await
        .expect("insert self-heal task");
        row.get("id")
    }

    #[tokio::test]
    async fn missing_signature_never_rearms() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let check = signature_should_rearm(&pool, "sig-missing", None)
            .await
            .expect("check missing signature");
        assert!(!check.should_rearm);
        assert!(check.existing_task_id.is_none());

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn completed_signature_after_cooldown_should_rearm() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let completed = Utc::now() - chrono::Duration::hours(1);
        insert_self_heal_task(&pool, "sig-old-completed", "completed", Some(completed)).await;

        let check = signature_should_rearm(&pool, "sig-old-completed", None)
            .await
            .expect("check completed signature");
        assert!(check.should_rearm);
        assert_eq!(check.terminal_status.as_deref(), Some("completed"));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn completed_signature_inside_cooldown_should_not_rearm() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let completed = Utc::now() - chrono::Duration::minutes(5);
        insert_self_heal_task(&pool, "sig-recent-completed", "completed", Some(completed)).await;

        let check = signature_should_rearm(&pool, "sig-recent-completed", None)
            .await
            .expect("check completed signature");
        assert!(!check.should_rearm);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn non_terminal_signature_should_not_rearm() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_self_heal_task(&pool, "sig-pending", "pending", None).await;

        let check = signature_should_rearm(&pool, "sig-pending", None)
            .await
            .expect("check pending signature");
        assert!(!check.should_rearm);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn rearm_task_resets_terminal_row_to_pending() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping rearm_self_heal_task DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let completed = Utc::now() - chrono::Duration::hours(1);
        let id = insert_self_heal_task(&pool, "sig-rearm", "completed", Some(completed)).await;

        // Populate fields that should be cleared on rearm.
        sqlx::query(
            "UPDATE fleet_tasks
             SET payload = payload || jsonb_build_object(
                 'last_attempt_at', NOW()::text,
                 'writer_computer_id', $2::text,
                 'escalated_to_operator_at', NOW()::text
             )
             WHERE id = $1",
        )
        .bind(id)
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .expect("seed rearm test fields");

        let before_rearm: DateTime<Utc> =
            sqlx::query_scalar("SELECT created_at FROM fleet_tasks WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("fetch pre-rearm created_at");

        let rearmed = rearm_self_heal_task(&pool, "sig-rearm", "T2", 3, None)
            .await
            .expect("rearm task");
        assert!(rearmed);

        let row = sqlx::query(
            "SELECT status,
                    created_at,
                    completed_at,
                    (payload->>'report_count')::int AS report_count,
                    (payload->>'status')::text AS payload_status,
                    (payload->>'attempts')::int AS attempts,
                    payload->>'last_attempt_at' AS last_attempt_at,
                    payload->>'writer_computer_id' AS writer_computer_id,
                    payload->>'escalated_to_operator_at' AS escalated_to_operator_at
               FROM fleet_tasks
              WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch rearmed task");
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<String, _>("payload_status"), "detected");
        assert_eq!(row.get::<i32, _>("report_count"), 3);
        assert_eq!(row.get::<i32, _>("attempts"), 0);
        assert!(
            row.try_get::<Option<DateTime<Utc>>, _>("completed_at")
                .ok()
                .flatten()
                .is_none()
        );
        assert!(row.get::<DateTime<Utc>, _>("created_at") > before_rearm);
        assert!(
            row.try_get::<Option<String>, _>("last_attempt_at")
                .ok()
                .flatten()
                .is_none()
        );
        assert!(
            row.try_get::<Option<String>, _>("writer_computer_id")
                .ok()
                .flatten()
                .is_none()
        );
        assert!(
            row.try_get::<Option<String>, _>("escalated_to_operator_at")
                .ok()
                .flatten()
                .is_none()
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn recurring_failed_task_rearms_after_cooldown() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping rearm_self_heal_task DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let failed_at = Utc::now() - chrono::Duration::hours(1);
        let id =
            insert_self_heal_task(&pool, "sig-recurring-failure", "failed", Some(failed_at)).await;

        let rearmed = rearm_self_heal_task(&pool, "sig-recurring-failure", "T1", 2, None)
            .await
            .expect("rearm recurring failed task");
        assert!(rearmed);

        let row = sqlx::query(
            "SELECT id, status, completed_at, payload->>'status' AS payload_status,
                    (payload->>'attempts')::int AS attempts
               FROM fleet_tasks
              WHERE dedup_signature = $1",
        )
        .bind("sig-recurring-failure")
        .fetch_one(&pool)
        .await
        .expect("fetch recurring rearmed task");

        assert_eq!(row.get::<uuid::Uuid, _>("id"), id);
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<String, _>("payload_status"), "detected");
        assert_eq!(row.get::<i32, _>("attempts"), 0);
        assert!(
            row.get::<Option<DateTime<Utc>>, _>("completed_at")
                .is_none()
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn rearm_task_is_no_op_for_active_row() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping rearm_self_heal_task DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_self_heal_task(&pool, "sig-no-rearm", "running", None).await;

        let rearmed = rearm_self_heal_task(&pool, "sig-no-rearm", "T2", 1, None)
            .await
            .expect("rearm task");
        assert!(!rearmed);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn scan_daemon_logs_creates_self_heal_task() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!("skipping scan_daemon_logs DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let mut log_file = tempfile::NamedTempFile::new().expect("create temp log");
        for i in 0..3 {
            writeln!(
                log_file,
                "2026-07-19 12:0{i}:00Z daemon[123{i}]: ERROR connection failed to 10.0.0.{i}:8080"
            )
            .unwrap();
        }
        let path = log_file.path().to_string_lossy().to_string();

        let config = DaemonLogScanConfig {
            paths: vec![path],
            patterns: vec!["ERROR".to_string()],
            min_recurrence: 3,
            tail_lines: 1000,
            interval_secs: 0,
        };

        let enqueued = scan_daemon_logs_with_config(&pool, "test-node", &config)
            .await
            .expect("scan daemon logs");
        assert_eq!(enqueued, 1);

        let row = sqlx::query(
            "SELECT status, (payload->>'report_count')::int AS report_count, \
                    payload->>'tier' AS tier, dedup_signature, \
                    payload->>'log_example' AS log_example \
               FROM fleet_tasks \
              WHERE task_class = 'self_heal' AND dedup_signature LIKE 'log:%'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch self-heal task");

        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<i32, _>("report_count"), 3);
        assert_eq!(row.get::<String, _>("tier"), "T2");
        assert!(row.get::<String, _>("dedup_signature").starts_with("log:"));
        let example: String = row.get("log_example");
        assert!(example.contains("connection failed"));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn scan_daemon_logs_skips_below_threshold() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!("skipping scan_daemon_logs DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let mut log_file = tempfile::NamedTempFile::new().expect("create temp log");
        writeln!(
            log_file,
            "2026-07-19 12:00:00Z daemon[1234]: ERROR connection failed to 10.0.0.1:8080"
        )
        .unwrap();
        writeln!(
            log_file,
            "2026-07-19 12:01:00Z daemon[5678]: ERROR connection failed to 10.0.0.2:9090"
        )
        .unwrap();
        let path = log_file.path().to_string_lossy().to_string();

        let config = DaemonLogScanConfig {
            paths: vec![path],
            patterns: vec!["ERROR".to_string()],
            min_recurrence: 3,
            tail_lines: 1000,
            interval_secs: 0,
        };

        let enqueued = scan_daemon_logs_with_config(&pool, "test-node", &config)
            .await
            .expect("scan daemon logs");
        assert_eq!(enqueued, 0);

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fleet_tasks WHERE task_class = 'self_heal' AND dedup_signature LIKE 'log:%'",
        )
        .fetch_one(&pool)
        .await
        .expect("count self-heal tasks");
        assert_eq!(count, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn scan_daemon_logs_rearms_terminal_signature() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!("skipping scan_daemon_logs DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let mut log_file = tempfile::NamedTempFile::new().expect("create temp log");
        for i in 0..3 {
            writeln!(
                log_file,
                "2026-07-19 12:0{i}:00Z daemon[123{i}]: ERROR connection failed to 10.0.0.{i}:8080"
            )
            .unwrap();
        }
        let path = log_file.path().to_string_lossy().to_string();

        let config = DaemonLogScanConfig {
            paths: vec![path.clone()],
            patterns: vec!["ERROR".to_string()],
            min_recurrence: 3,
            tail_lines: 1000,
            interval_secs: 0,
        };

        // First scan creates the task.
        let enqueued = scan_daemon_logs_with_config(&pool, "test-node", &config)
            .await
            .expect("scan daemon logs");
        assert_eq!(enqueued, 1);

        let sig: String = sqlx::query_scalar(
            "SELECT dedup_signature FROM fleet_tasks WHERE task_class = 'self_heal' AND dedup_signature LIKE 'log:%'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch signature");

        // Mark it terminal and well past the re-arm cooldown.
        sqlx::query(
            "UPDATE fleet_tasks \
             SET status = 'failed', \
                 completed_at = NOW() - INTERVAL '1 hour', \
                 payload = payload || jsonb_build_object( \
                     'status', 'failed', \
                     'attempts', 2, \
                     'last_attempt_at', NOW(), \
                     'writer_computer_id', $2::text, \
                     'escalated_to_operator_at', NOW() \
                 ) \
             WHERE task_class = 'self_heal' AND dedup_signature = $1",
        )
        .bind(&sig)
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .expect("mark self-heal task terminal");

        // Second scan re-arms the cooled terminal signature.
        let enqueued = scan_daemon_logs_with_config(&pool, "test-node", &config)
            .await
            .expect("scan daemon logs again");
        assert_eq!(enqueued, 1);

        let row = sqlx::query(
            "SELECT status, payload->>'status' AS payload_status, \
                    (payload->>'attempts')::int AS attempts, \
                    payload->>'writer_computer_id' AS writer_computer_id \
               FROM fleet_tasks \
              WHERE task_class = 'self_heal' AND dedup_signature = $1",
        )
        .bind(&sig)
        .fetch_one(&pool)
        .await
        .expect("fetch rearmed task");

        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<String, _>("payload_status"), "detected");
        assert_eq!(row.get::<i32, _>("attempts"), 0);
        assert!(
            row.try_get::<Option<String>, _>("writer_computer_id")
                .ok()
                .flatten()
                .is_none()
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[test]
    fn self_heal_state_starts_empty() {
        let state = SelfHealState::new();
        assert!(state.tracked_signatures.is_empty());
        assert!(!state.is_tracked("sig"));
    }

    #[test]
    fn self_heal_state_tracks_signatures() {
        let mut state = SelfHealState::new();
        assert!(state.track("sig-a"));
        assert!(state.is_tracked("sig-a"));
        assert!(!state.track("sig-a"));
        assert!(state.track("sig-b"));
        assert_eq!(state.tracked_signatures.len(), 2);
    }

    #[test]
    fn self_heal_state_detects_reappearing_bug() {
        let mut state = SelfHealState::new();
        state.track("recurring-sig");

        assert!(state.is_reappearing("recurring-sig", "detected"));
        assert!(!state.is_reappearing("recurring-sig", "fixing"));
        assert!(!state.is_reappearing("recurring-sig", "verified"));
        assert!(!state.is_reappearing("unknown-sig", "detected"));
    }
}
