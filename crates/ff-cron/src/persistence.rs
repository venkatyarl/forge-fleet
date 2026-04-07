//! OperationalStore-backed persistence for cron jobs and run history.
//!
//! Uses the `ff-db` schema (tables `cron_jobs` and `cron_runs`) while supporting
//! both embedded SQLite and Postgres backends.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use ff_db::{DbPool, DbPoolConfig, OperationalStore, run_migrations};
use rusqlite::{OptionalExtension, params};
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

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

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
    store: OperationalStore,
    sqlite_path: Option<PathBuf>,
}

impl CronPersistence {
    /// Create persistence store at the provided embedded SQLite file path.
    ///
    /// This compatibility constructor keeps `embedded_sqlite` behavior intact.
    pub async fn new(path: impl Into<PathBuf>) -> Result<Self, PersistenceError> {
        let db_path = path.into();

        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                PersistenceError::Decode(format!(
                    "failed to create db directory {}: {err}",
                    parent.display()
                ))
            })?;
        }

        let pool = DbPool::open(DbPoolConfig::with_path(&db_path))?;
        let conn = pool.open_raw_connection()?;
        run_migrations(&conn)?;

        Ok(Self {
            store: OperationalStore::sqlite(pool),
            sqlite_path: Some(db_path),
        })
    }

    /// Build cron persistence on top of an existing operational store.
    pub async fn from_operational_store(store: OperationalStore) -> Result<Self, PersistenceError> {
        let sqlite_path = if let Some(pool) = store.sqlite_pool() {
            let conn = pool.open_raw_connection()?;
            run_migrations(&conn)?;
            Some(pool.path().to_path_buf())
        } else {
            None
        };

        Ok(Self { store, sqlite_path })
    }

    /// Optional path to the underlying SQLite database file.
    ///
    /// Returns `None` when persistence is Postgres-backed.
    pub fn path(&self) -> Option<&Path> {
        self.sqlite_path.as_deref()
    }

    pub fn backend_label(&self) -> &'static str {
        self.store.backend_label()
    }

    // ── Jobs ──────────────────────────────────────────────────────────────

    /// Insert or update a cron job.
    pub async fn upsert_job(&self, job: &JobDefinition) -> Result<(), PersistenceError> {
        let payload = serde_json::to_string(job)?;
        let task_kind = task_kind_label(&job.task);

        match &self.store {
            OperationalStore::Sqlite(pool) => {
                let id = job.id.to_string();
                let name = job.name.clone();
                let schedule = job.schedule_expression.clone();
                let task_kind = task_kind.to_string();
                let payload = payload;
                let enabled = job.enabled;
                let node_affinity = job.ownership.owner_node.clone();
                let created_at = to_rfc3339(job.created_at);
                let updated_at = to_rfc3339(job.updated_at);

                pool.with_conn(move |conn| {
                    conn.execute(
                        r#"
                        INSERT INTO cron_jobs (
                            id, name, schedule, task_kind, payload_json,
                            enabled, node_affinity, created_at, updated_at
                        )
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                        ON CONFLICT(id) DO UPDATE SET
                            name = excluded.name,
                            schedule = excluded.schedule,
                            task_kind = excluded.task_kind,
                            payload_json = excluded.payload_json,
                            enabled = excluded.enabled,
                            node_affinity = excluded.node_affinity,
                            updated_at = excluded.updated_at
                        "#,
                        params![
                            id,
                            name,
                            schedule,
                            task_kind,
                            payload,
                            enabled as i64,
                            node_affinity,
                            created_at,
                            updated_at,
                        ],
                    )?;
                    Ok(())
                })
                .await?;
            }
            OperationalStore::Postgres(pool) => {
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
                .execute(pool.as_ref())
                .await?;
            }
        }

        Ok(())
    }

    /// Fetch a single job by ID.
    pub async fn get_job(&self, id: Uuid) -> Result<Option<JobDefinition>, PersistenceError> {
        let id_raw = id.to_string();

        let row = match &self.store {
            OperationalStore::Sqlite(pool) => {
                let id_raw = id_raw.clone();
                pool.with_conn(move |conn| {
                    let row = conn
                        .query_row(
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
                            WHERE id = ?1
                            "#,
                            [id_raw],
                            |row| {
                                Ok(PersistedJobRow {
                                    id: row.get(0)?,
                                    name: row.get(1)?,
                                    schedule: row.get(2)?,
                                    payload_json: row.get(3)?,
                                    enabled: row.get::<_, i64>(4)? != 0,
                                    node_affinity: row.get(5)?,
                                    created_at: row.get(6)?,
                                    updated_at: row.get(7)?,
                                })
                            },
                        )
                        .optional()?;
                    Ok(row)
                })
                .await?
            }
            OperationalStore::Postgres(pool) => {
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
                .fetch_optional(pool.as_ref())
                .await?;

                match row {
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
                }
            }
        };

        row.map(decode_job_row).transpose()
    }

    /// List jobs. When `active_only` is true, only enabled jobs are returned.
    pub async fn list_jobs(
        &self,
        active_only: bool,
    ) -> Result<Vec<JobDefinition>, PersistenceError> {
        let rows: Vec<PersistedJobRow> = match &self.store {
            OperationalStore::Sqlite(pool) => {
                pool.with_conn(move |conn| {
                    let sql = if active_only {
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
                        WHERE enabled = 1
                        ORDER BY updated_at DESC
                        "#
                    } else {
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
                        "#
                    };

                    let mut stmt = conn.prepare(sql)?;
                    let rows = stmt.query_map([], |row| {
                        Ok(PersistedJobRow {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            schedule: row.get(2)?,
                            payload_json: row.get(3)?,
                            enabled: row.get::<_, i64>(4)? != 0,
                            node_affinity: row.get(5)?,
                            created_at: row.get(6)?,
                            updated_at: row.get(7)?,
                        })
                    })?;

                    let mut collected = Vec::new();
                    for row in rows {
                        collected.push(row?);
                    }
                    Ok(collected)
                })
                .await?
            }
            OperationalStore::Postgres(pool) => {
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
                    .fetch_all(pool.as_ref())
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
                    .fetch_all(pool.as_ref())
                    .await?
                };

                let mut collected = Vec::with_capacity(rows.len());
                for row in rows {
                    collected.push(PersistedJobRow {
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
                collected
            }
        };

        let mut jobs = Vec::with_capacity(rows.len());
        for row in rows {
            jobs.push(decode_job_row(row)?);
        }

        Ok(jobs)
    }

    /// Delete a job by ID. Returns true if a row was deleted.
    pub async fn delete_job(&self, id: Uuid) -> Result<bool, PersistenceError> {
        let id_raw = id.to_string();

        let deleted = match &self.store {
            OperationalStore::Sqlite(pool) => {
                let id_raw = id_raw.clone();
                pool.with_conn(move |conn| {
                    let affected = conn.execute("DELETE FROM cron_jobs WHERE id = ?1", [id_raw])?;
                    Ok(affected > 0)
                })
                .await?
            }
            OperationalStore::Postgres(pool) => {
                let affected = sqlx::query("DELETE FROM cron_jobs WHERE id = $1")
                    .bind(id_raw)
                    .execute(pool.as_ref())
                    .await?
                    .rows_affected();
                affected > 0
            }
        };

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

        match &self.store {
            OperationalStore::Sqlite(pool) => {
                let success_sqlite = success.map(|value| if value { 1_i64 } else { 0_i64 });
                let started_at_sqlite = started_at.clone();
                let completed_at_update = completed_at.clone();
                let completed_at_insert = completed_at.clone();
                let output_update = output.clone();
                let output_insert = output.clone();
                let job_id_sqlite = job_id.clone();
                let job_id_insert = job_id.clone();

                pool.with_conn_mut(move |conn| {
                    let tx = conn.transaction()?;

                    let updated = tx.execute(
                        r#"
                        UPDATE cron_runs
                        SET completed_at = ?1,
                            success = ?2,
                            output = ?3,
                            duration_ms = ?4
                        WHERE id = (
                            SELECT id
                            FROM cron_runs
                            WHERE cron_job_id = ?5 AND completed_at IS NULL
                            ORDER BY id DESC
                            LIMIT 1
                        )
                        "#,
                        params![
                            completed_at_update,
                            success_sqlite,
                            output_update,
                            duration_ms,
                            job_id_sqlite,
                        ],
                    )?;

                    if updated == 0 {
                        tx.execute(
                            r#"
                            INSERT INTO cron_runs (
                                cron_job_id, started_at, completed_at, success, output, duration_ms
                            )
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                            "#,
                            params![
                                job_id_insert,
                                started_at_sqlite,
                                completed_at_insert,
                                success_sqlite,
                                output_insert,
                                duration_ms,
                            ],
                        )?;
                    }

                    tx.commit()?;
                    Ok(())
                })
                .await?;
            }
            OperationalStore::Postgres(pool) => {
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
                .execute(pool.as_ref())
                .await?;
            }
        }

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

        let rows: Vec<PersistedRunRow> = match &self.store {
            OperationalStore::Sqlite(pool) => {
                let job_id = job_id.map(|value| value.to_string());
                pool.with_conn(move |conn| {
                    let mut runs = Vec::new();

                    if let Some(job_id) = job_id {
                        let mut stmt = conn.prepare(
                            r#"
                            SELECT cron_job_id, started_at, completed_at, success, output
                            FROM cron_runs
                            WHERE cron_job_id = ?1
                            ORDER BY id DESC
                            LIMIT ?2
                            "#,
                        )?;

                        let rows = stmt.query_map(params![job_id, limit], |row| {
                            Ok(PersistedRunRow {
                                cron_job_id: row.get(0)?,
                                started_at: row.get(1)?,
                                completed_at: row.get(2)?,
                                success: row.get::<_, Option<i64>>(3)?.map(|value| value != 0),
                                output: row.get(4)?,
                            })
                        })?;

                        for row in rows {
                            runs.push(row?);
                        }
                    } else {
                        let mut stmt = conn.prepare(
                            r#"
                            SELECT cron_job_id, started_at, completed_at, success, output
                            FROM cron_runs
                            ORDER BY id DESC
                            LIMIT ?1
                            "#,
                        )?;

                        let rows = stmt.query_map([limit], |row| {
                            Ok(PersistedRunRow {
                                cron_job_id: row.get(0)?,
                                started_at: row.get(1)?,
                                completed_at: row.get(2)?,
                                success: row.get::<_, Option<i64>>(3)?.map(|value| value != 0),
                                output: row.get(4)?,
                            })
                        })?;

                        for row in rows {
                            runs.push(row?);
                        }
                    }

                    Ok(runs)
                })
                .await?
            }
            OperationalStore::Postgres(pool) => {
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
                    .fetch_all(pool.as_ref())
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
                    .fetch_all(pool.as_ref())
                    .await?
                };

                let mut collected = Vec::with_capacity(rows.len());
                for row in rows {
                    collected.push(PersistedRunRow {
                        cron_job_id: row.try_get("cron_job_id")?,
                        started_at: row.try_get("started_at")?,
                        completed_at: row.try_get("completed_at")?,
                        success: row.try_get("success")?,
                        output: row.try_get("output")?,
                    });
                }
                collected
            }
        };

        let mut runs = Vec::with_capacity(rows.len());
        for row in rows {
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

        let deleted = match &self.store {
            OperationalStore::Sqlite(pool) => {
                let cutoff = cutoff.clone();
                pool.with_conn(move |conn| {
                    let deleted =
                        conn.execute("DELETE FROM cron_runs WHERE started_at < ?1", [cutoff])?;
                    Ok(deleted as u64)
                })
                .await?
            }
            OperationalStore::Postgres(pool) => {
                sqlx::query("DELETE FROM cron_runs WHERE started_at < $1")
                    .bind(cutoff)
                    .execute(pool.as_ref())
                    .await?
                    .rows_affected()
            }
        };

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
        .map_err(|err| PersistenceError::Decode(format!("invalid timestamp '{}': {err}", value)))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobTask, RetryPolicy};
    use crate::policy::{BackoffPolicy, JobPriority};

    #[tokio::test]
    async fn save_load_delete_job_roundtrip() {
        let db_path = std::env::temp_dir().join(format!("ff-cron-{}.db", Uuid::new_v4()));
        let persistence = CronPersistence::new(&db_path).await.unwrap();

        let mut job = JobDefinition::new(
            "heartbeat-check",
            "*/10 * * * *",
            JobTask::LocalCommand {
                command: "echo ok".into(),
                timeout_secs: Some(30),
            },
            JobPriority::High,
        )
        .unwrap();

        job.retry = RetryPolicy {
            max_attempts: 5,
            backoff: BackoffPolicy::default(),
        };
        job.metadata.tags = vec!["health".into()];

        persistence.upsert_job(&job).await.unwrap();

        let loaded = persistence.get_job(job.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, job.id);
        assert_eq!(loaded.name, "heartbeat-check");
        assert_eq!(loaded.schedule_expression, "*/10 * * * *");
        assert_eq!(loaded.priority, JobPriority::High);
        assert_eq!(loaded.retry.max_attempts, 5);
        assert_eq!(loaded.metadata.tags, vec!["health".to_string()]);

        let active = persistence.list_jobs(true).await.unwrap();
        assert_eq!(active.len(), 1);

        let deleted = persistence.delete_job(job.id).await.unwrap();
        assert!(deleted);

        let after_delete = persistence.get_job(job.id).await.unwrap();
        assert!(after_delete.is_none());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn from_operational_store_sqlite_backend_roundtrip() {
        let db_path = std::env::temp_dir().join(format!("ff-cron-store-{}.db", Uuid::new_v4()));
        let pool = DbPool::open(DbPoolConfig::with_path(&db_path)).unwrap();
        let persistence = CronPersistence::from_operational_store(OperationalStore::sqlite(pool))
            .await
            .unwrap();

        assert_eq!(persistence.backend_label(), "embedded_sqlite");
        assert_eq!(persistence.path(), Some(db_path.as_path()));

        let job = JobDefinition::new(
            "store-roundtrip",
            "*/15 * * * *",
            JobTask::LocalCommand {
                command: "echo from store".into(),
                timeout_secs: Some(15),
            },
            JobPriority::Normal,
        )
        .unwrap();

        persistence.upsert_job(&job).await.unwrap();
        let loaded = persistence.get_job(job.id).await.unwrap().unwrap();
        assert_eq!(loaded.name, "store-roundtrip");

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn persist_run_and_read_back() {
        let db_path = std::env::temp_dir().join(format!("ff-cron-runs-{}.db", Uuid::new_v4()));
        let persistence = CronPersistence::new(&db_path).await.unwrap();

        let job = JobDefinition::new(
            "nightly",
            "0 2 * * *",
            JobTask::LocalCommand {
                command: "echo nightly".into(),
                timeout_secs: None,
            },
            JobPriority::Normal,
        )
        .unwrap();

        persistence.upsert_job(&job).await.unwrap();

        let mut run = JobRun::pending(job.id, Utc::now(), 1);
        run.mark_running(None);
        persistence.upsert_run(&run).await.unwrap();

        run.mark_success("done".into());
        persistence.upsert_run(&run).await.unwrap();

        let runs = persistence.list_runs(Some(job.id), 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Succeeded);
        assert_eq!(runs[0].attempt, 1);
        assert!(runs[0].output.clone().unwrap_or_default().contains("done"));

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn list_all_jobs_includes_disabled() {
        let db_path = std::env::temp_dir().join(format!("ff-cron-all-{}.db", Uuid::new_v4()));
        let persistence = CronPersistence::new(&db_path).await.unwrap();

        let enabled_job = JobDefinition::new(
            "enabled-job",
            "*/5 * * * *",
            JobTask::LocalCommand {
                command: "echo on".into(),
                timeout_secs: None,
            },
            JobPriority::Normal,
        )
        .unwrap();

        let mut disabled_job = JobDefinition::new(
            "disabled-job",
            "0 * * * *",
            JobTask::LocalCommand {
                command: "echo off".into(),
                timeout_secs: None,
            },
            JobPriority::Low,
        )
        .unwrap();
        disabled_job.enabled = false;

        persistence.upsert_job(&enabled_job).await.unwrap();
        persistence.upsert_job(&disabled_job).await.unwrap();

        let active = persistence.list_jobs(true).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "enabled-job");

        let all = persistence.list_jobs(false).await.unwrap();
        assert_eq!(all.len(), 2);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn cleanup_old_runs() {
        let db_path = std::env::temp_dir().join(format!("ff-cron-cleanup-{}.db", Uuid::new_v4()));
        let persistence = CronPersistence::new(&db_path).await.unwrap();

        let job = JobDefinition::new(
            "cleanup-test",
            "* * * * *",
            JobTask::LocalCommand {
                command: "echo hi".into(),
                timeout_secs: None,
            },
            JobPriority::Normal,
        )
        .unwrap();

        persistence.upsert_job(&job).await.unwrap();

        let run = JobRun::pending(job.id, Utc::now(), 1);
        persistence.upsert_run(&run).await.unwrap();

        // Should not delete recent runs.
        let deleted = persistence.cleanup_runs_older_than(1).await.unwrap();
        assert_eq!(deleted, 0);

        let runs = persistence.list_runs(Some(job.id), 10).await.unwrap();
        assert_eq!(runs.len(), 1);

        let _ = std::fs::remove_file(db_path);
    }
}
