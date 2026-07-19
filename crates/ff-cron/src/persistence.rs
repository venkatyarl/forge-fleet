//! OperationalStore-backed persistence for cron jobs and run history.
//!
//! Uses the `ff-db` Postgres schema (tables `cron_jobs` and `cron_runs`).

use chrono::{DateTime, Duration, Utc};
use ff_db::OperationalStore;
use sqlx::PgPool;
use sqlx::Row;
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

use crate::job::{JobDefinition, JobRun, RunStatus};

/// Prefix written into the `output` column of `cron_runs` so we can
/// round-trip extra metadata (run ID, attempt, status string) through
/// a schema that only stores `output TEXT`.
const RUN_META_PREFIX: &str = "__ffcron_meta__";

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("ff-db error: {0}")]
    Db(#[from] ff_db::DbError),

    #[error("postgres/sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("decode error: {0}")]
    Decode(String),
}

// ─── CronPersistence ─────────────────────────────────────────────────────────

/// Operational-store-backed persistence for cron jobs and run history.
#[derive(Clone, Debug)]
pub struct CronPersistence {
    pool: PgPool,
}

impl CronPersistence {
    /// Build cron persistence on top of an existing operational store.
    pub async fn from_operational_store(store: OperationalStore) -> Result<Self, PersistenceError> {
        let pool = store.pg_pool().ok_or_else(|| {
            PersistenceError::Decode(
                "cron persistence requires a Postgres operational store".into(),
            )
        })?;

        Ok(Self { pool: pool.clone() })
    }

    pub fn backend_label(&self) -> &'static str {
        "postgres"
    }

    // ── Jobs ──────────────────────────────────────────────────────────────

    /// Insert or update a cron job.
    pub async fn upsert_job(&self, job: &JobDefinition) -> Result<(), PersistenceError> {
        let payload = serde_json::to_string(job)?;
        let task_kind = task_kind_label(&job.task);

        sqlx::query(
            r#"
            INSERT INTO cron_jobs (
                id, name, schedule, task_kind, payload_json,
                enabled, node_affinity, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT(id) DO UPDATE SET
                name = EXCLUDED.name,
                schedule = EXCLUDED.schedule,
                task_kind = EXCLUDED.task_kind,
                payload_json = EXCLUDED.payload_json,
                enabled = EXCLUDED.enabled,
                node_affinity = EXCLUDED.node_affinity,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(job.id.to_string())
        .bind(&job.name)
        .bind(&job.schedule_expression)
        .bind(task_kind)
        .bind(payload)
        .bind(job.enabled)
        .bind(job.ownership.owner_node.as_deref())
        .bind(to_rfc3339(job.created_at))
        .bind(to_rfc3339(job.updated_at))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Fetch a single job by ID.
    pub async fn get_job(&self, id: Uuid) -> Result<Option<JobDefinition>, PersistenceError> {
        let id_raw = id.to_string();

        let row = sqlx::query(
            r#"
            SELECT
                id,
                name,
                schedule,
                payload_json,
                enabled,
                node_affinity,
                created_at,
                updated_at
            FROM cron_jobs
            WHERE id = $1
            "#,
        )
        .bind(id_raw)
        .fetch_optional(&self.pool)
        .await?;

        let row = match row {
            Some(row) => Some(PersistedJobRow {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                schedule: row.try_get("schedule")?,
                payload_json: row.try_get("payload_json")?,
                enabled: row.try_get("enabled")?,
                node_affinity: row.try_get("node_affinity")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            }),
            None => None,
        };

        row.map(decode_job_row).transpose()
    }

    /// List jobs. When `active_only` is true, only enabled jobs are returned.
    pub async fn list_jobs(
        &self,
        active_only: bool,
    ) -> Result<Vec<JobDefinition>, PersistenceError> {
        let rows = if active_only {
            sqlx::query(
                r#"
                SELECT
                    id,
                    name,
                    schedule,
                    payload_json,
                    enabled,
                    node_affinity,
                    created_at,
                    updated_at
                FROM cron_jobs
                WHERE enabled = TRUE
                ORDER BY updated_at DESC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT
                    id,
                    name,
                    schedule,
                    payload_json,
                    enabled,
                    node_affinity,
                    created_at,
                    updated_at
                FROM cron_jobs
                ORDER BY updated_at DESC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };

        let mut rows_out = Vec::with_capacity(rows.len());
        for row in rows {
            rows_out.push(PersistedJobRow {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                schedule: row.try_get("schedule")?,
                payload_json: row.try_get("payload_json")?,
                enabled: row.try_get("enabled")?,
                node_affinity: row.try_get("node_affinity")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            });
        }

        let mut jobs = Vec::with_capacity(rows_out.len());
        for row in rows_out {
            jobs.push(decode_job_row(row)?);
        }

        Ok(jobs)
    }

    /// Delete a job by ID. Returns true if a row was deleted.
    pub async fn delete_job(&self, id: Uuid) -> Result<bool, PersistenceError> {
        let id_raw = id.to_string();

        let deleted = sqlx::query("DELETE FROM cron_jobs WHERE id = $1")
            .bind(id_raw)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0;

        Ok(deleted)
    }

    // ── Runs ──────────────────────────────────────────────────────────────

    /// Insert or update a run record in `cron_runs`.
    pub async fn upsert_run(&self, run: &JobRun) -> Result<(), PersistenceError> {
        let status = run.status.as_str().to_string();
        let success = run_status_to_success(run.status);
        let started_at = to_rfc3339(run.started_at.unwrap_or(run.scheduled_for));
        let completed_at = run.finished_at.map(to_rfc3339);
        let duration_ms = run
            .started_at
            .zip(run.finished_at)
            .map(|(start, end)| (end - start).num_milliseconds().max(0));
        let output = encode_run_output(run);
        let job_id = run.job_id.to_string();

        sqlx::query(
            r#"
            WITH updated AS (
                UPDATE cron_runs
                SET completed_at = $1,
                    success = $2,
                    output = $3,
                    duration_ms = $4
                WHERE id = (
                    SELECT id
                    FROM cron_runs
                    WHERE cron_job_id = $5 AND completed_at IS NULL
                    ORDER BY id DESC
                    LIMIT 1
                )
                RETURNING id
            )
            INSERT INTO cron_runs (
                cron_job_id,
                started_at,
                completed_at,
                success,
                output,
                duration_ms
            )
            SELECT $5, $6, $1, $2, $3, $4
            WHERE NOT EXISTS (SELECT 1 FROM updated)
            "#,
        )
        .bind(completed_at.as_deref())
        .bind(success)
        .bind(&output)
        .bind(duration_ms)
        .bind(&job_id)
        .bind(&started_at)
        .execute(&self.pool)
        .await?;

        debug!(
            run_id = %run.id,
            job_id = %run.job_id,
            status,
            backend = self.backend_label(),
            "cron run persisted"
        );
        Ok(())
    }

    /// List recent runs, optionally filtered to a specific job.
    pub async fn list_runs(
        &self,
        job_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<JobRun>, PersistenceError> {
        let limit = limit.clamp(1, 1000) as i64;

        let rows = if let Some(job_id) = job_id {
            sqlx::query(
                r#"
                SELECT cron_job_id, started_at, completed_at, success, output
                FROM cron_runs
                WHERE cron_job_id = $1
                ORDER BY id DESC
                LIMIT $2
                "#,
            )
            .bind(job_id.to_string())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT cron_job_id, started_at, completed_at, success, output
                FROM cron_runs
                ORDER BY id DESC
                LIMIT $1
                "#,
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        let mut persisted_rows = Vec::with_capacity(rows.len());
        for row in rows {
            persisted_rows.push(PersistedRunRow {
                cron_job_id: row.try_get("cron_job_id")?,
                started_at: row.try_get("started_at")?,
                completed_at: row.try_get("completed_at")?,
                success: row.try_get("success")?,
                output: row.try_get("output")?,
            });
        }

        let mut runs = Vec::with_capacity(persisted_rows.len());
        for row in persisted_rows {
            runs.push(decode_run_row(row)?);
        }

        Ok(runs)
    }

    /// Delete runs older than `retention_days`.
    pub async fn cleanup_runs_older_than(
        &self,
        retention_days: i64,
    ) -> Result<u64, PersistenceError> {
        let cutoff = Utc::now() - Duration::days(retention_days.max(1));
        let cutoff = to_rfc3339(cutoff);

        let deleted = sqlx::query("DELETE FROM cron_runs WHERE started_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();

        debug!(
            deleted,
            retention_days,
            backend = self.backend_label(),
            "deleted old cron run records"
        );
        Ok(deleted)
    }
}

// ─── Private helpers ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PersistedJobRow {
    id: String,
    name: String,
    schedule: String,
    payload_json: String,
    enabled: bool,
    node_affinity: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct PersistedRunRow {
    cron_job_id: String,
    started_at: String,
    completed_at: Option<String>,
    success: Option<bool>,
    output: String,
}

fn task_kind_label(task: &crate::job::JobTask) -> &'static str {
    match task {
        crate::job::JobTask::LocalCommand { .. } => "local_command",
        crate::job::JobTask::FleetTask { .. } => "fleet_task",
    }
}

fn decode_job_row(row: PersistedJobRow) -> Result<JobDefinition, PersistenceError> {
    let mut job: JobDefinition = serde_json::from_str(&row.payload_json)?;

    let parsed_id = Uuid::parse_str(&row.id).map_err(|err| {
        PersistenceError::Decode(format!("invalid cron_jobs.id '{}': {err}", row.id))
    })?;

    job.id = parsed_id;
    job.name = row.name;
    job.schedule_expression = row.schedule;
    job.enabled = row.enabled;
    job.ownership.owner_node = row.node_affinity;
    job.created_at = parse_rfc3339(&row.created_at)?;
    job.updated_at = parse_rfc3339(&row.updated_at)?;

    Ok(job)
}

fn encode_run_output(run: &JobRun) -> String {
    let mut body = String::new();

    if let Some(output) = &run.output {
        body.push_str(output.trim());
    }

    if let Some(error) = &run.error {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("error: ");
        body.push_str(error.trim());
    }

    format!(
        "{} run_id={} attempt={} status={}\n{}",
        RUN_META_PREFIX,
        run.id,
        run.attempt,
        run.status.as_str(),
        body
    )
}

fn decode_run_row(row: PersistedRunRow) -> Result<JobRun, PersistenceError> {
    let job_id = Uuid::parse_str(&row.cron_job_id).map_err(|err| {
        PersistenceError::Decode(format!(
            "invalid cron_runs.cron_job_id '{}': {err}",
            row.cron_job_id
        ))
    })?;

    let started_at = parse_rfc3339(&row.started_at)?;
    let finished_at = row.completed_at.as_deref().map(parse_rfc3339).transpose()?;

    let parsed_meta = parse_run_meta(&row.output);

    let id = parsed_meta
        .as_ref()
        .and_then(|m| m.run_id)
        .unwrap_or_else(Uuid::new_v4);

    let attempt = parsed_meta.as_ref().map(|m| m.attempt).unwrap_or(1);

    let status = parsed_meta
        .as_ref()
        .and_then(|m| RunStatus::parse_str(&m.status))
        .or_else(|| infer_status(row.success, finished_at.is_some()))
        .unwrap_or(RunStatus::Pending);

    let body = parsed_meta
        .as_ref()
        .map(|m| m.body.trim().to_string())
        .unwrap_or_else(|| row.output.trim().to_string());

    let (output, error) = if status == RunStatus::Failed {
        (None, if body.is_empty() { None } else { Some(body) })
    } else {
        (if body.is_empty() { None } else { Some(body) }, None)
    };

    Ok(JobRun {
        id,
        job_id,
        status,
        scheduled_for: started_at,
        attempt,
        worker: None,
        started_at: Some(started_at),
        finished_at,
        output,
        error,
        created_at: started_at,
    })
}

#[derive(Debug)]
struct ParsedRunMeta {
    run_id: Option<Uuid>,
    attempt: u32,
    status: String,
    body: String,
}

fn parse_run_meta(raw: &str) -> Option<ParsedRunMeta> {
    let mut lines = raw.lines();
    let header = lines.next()?.trim();

    if !header.starts_with(RUN_META_PREFIX) {
        return None;
    }

    let mut run_id = None;
    let mut attempt = 1;
    let mut status = String::from("pending");

    for token in header.split_whitespace().skip(1) {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "run_id" => run_id = Uuid::parse_str(value).ok(),
            "attempt" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    attempt = parsed.max(1);
                }
            }
            "status" => status = value.to_string(),
            _ => {}
        }
    }

    let body = lines.collect::<Vec<_>>().join("\n");

    Some(ParsedRunMeta {
        run_id,
        attempt,
        status,
        body,
    })
}

fn run_status_to_success(status: RunStatus) -> Option<bool> {
    match status {
        RunStatus::Pending | RunStatus::Running | RunStatus::Dispatched => None,
        RunStatus::Succeeded | RunStatus::Skipped => Some(true),
        RunStatus::Failed => Some(false),
    }
}

fn infer_status(success: Option<bool>, completed: bool) -> Option<RunStatus> {
    match (success, completed) {
        (Some(true), _) => Some(RunStatus::Succeeded),
        (Some(false), _) => Some(RunStatus::Failed),
        (None, true) => Some(RunStatus::Skipped),
        (None, false) => Some(RunStatus::Running),
    }
}

fn to_rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}

fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>, PersistenceError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| PersistenceError::Decode(format!("invalid timestamp '{value}': {err}")))
}
