//! Operational persistence abstraction (SQLite or Postgres).
//!
//! Unlike `RuntimeRegistryStore` (runtime heartbeat/enrollment tables only),
//! this store covers the broader operational tables used by live gateway/agent
//! paths: tasks, task_results, nodes, config_kv, audit_log, ownership leases,
//! autonomy events, and telegram ingest metadata.

use std::sync::Arc;

use chrono::{Duration, SecondsFormat, Utc};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tracing::info;

use crate::{
    DbPool,
    error::DbError,
    queries::{self, AuditLogRow, AutonomyEventRow, NodeRow, TaskRow},
};

#[derive(Debug, Clone)]
pub enum OperationalStore {
    /// Persist operational tables in embedded SQLite.
    Sqlite(DbPool),
    /// Persist operational tables in Postgres.
    Postgres(Arc<PgPool>),
}

impl OperationalStore {
    pub fn sqlite(pool: DbPool) -> Self {
        Self::Sqlite(pool)
    }

    pub async fn postgres(database_url: &str, max_connections: u32) -> Result<Self, DbError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .connect(database_url)
            .await?;

        let store = Self::Postgres(Arc::new(pool));
        store.ensure_postgres_schema().await?;
        Ok(store)
    }

    pub fn backend_label(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "embedded_sqlite",
            Self::Postgres(_) => "postgres",
        }
    }

    pub fn sqlite_pool(&self) -> Option<&DbPool> {
        match self {
            Self::Sqlite(pool) => Some(pool),
            Self::Postgres(_) => None,
        }
    }

    /// Get a reference to the Postgres pool, if this store is Postgres-backed.
    pub fn pg_pool(&self) -> Option<&PgPool> {
        match self {
            Self::Sqlite(_) => None,
            Self::Postgres(pool) => Some(pool.as_ref()),
        }
    }

    pub async fn health_probe(&self) -> Result<(bool, u64), DbError> {
        match self {
            Self::Sqlite(pool) => {
                pool.with_conn(|conn| {
                    let health: i64 = conn.query_row("SELECT 1", [], |row| row.get(0))?;
                    let kv_count: i64 =
                        conn.query_row("SELECT COUNT(*) FROM config_kv", [], |row| row.get(0))?;
                    Ok((health == 1, kv_count.max(0) as u64))
                })
                .await
            }
            Self::Postgres(pool) => {
                let health: i64 = sqlx::query_scalar("SELECT 1")
                    .fetch_one(pool.as_ref())
                    .await?;
                let kv_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM config_kv")
                    .fetch_one(pool.as_ref())
                    .await?;
                Ok((health == 1, kv_count.max(0) as u64))
            }
        }
    }

    pub async fn upsert_node(&self, node: &NodeRow) -> Result<(), DbError> {
        match self {
            Self::Sqlite(pool) => {
                let node = node.clone();
                pool.with_conn(move |conn| queries::upsert_node(conn, &node))
                    .await
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    r#"
                    INSERT INTO nodes (
                        id,
                        name,
                        host,
                        port,
                        role,
                        election_priority,
                        status,
                        hardware_json,
                        models_json,
                        last_heartbeat,
                        registered_at
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11
                    )
                    ON CONFLICT(id) DO UPDATE SET
                        name = EXCLUDED.name,
                        host = EXCLUDED.host,
                        port = EXCLUDED.port,
                        role = EXCLUDED.role,
                        election_priority = EXCLUDED.election_priority,
                        status = EXCLUDED.status,
                        hardware_json = EXCLUDED.hardware_json,
                        models_json = EXCLUDED.models_json,
                        last_heartbeat = EXCLUDED.last_heartbeat
                    "#,
                )
                .bind(&node.id)
                .bind(&node.name)
                .bind(&node.host)
                .bind(node.port)
                .bind(&node.role)
                .bind(node.election_priority)
                .bind(&node.status)
                .bind(&node.hardware_json)
                .bind(&node.models_json)
                .bind(node.last_heartbeat.as_deref())
                .bind(&node.registered_at)
                .execute(pool.as_ref())
                .await?;
                Ok(())
            }
        }
    }

    pub async fn list_nodes(&self) -> Result<Vec<NodeRow>, DbError> {
        match self {
            Self::Sqlite(pool) => pool.with_conn(queries::list_nodes).await,
            Self::Postgres(pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        id,
                        name,
                        host,
                        port,
                        role,
                        election_priority,
                        status,
                        hardware_json,
                        models_json,
                        last_heartbeat,
                        registered_at
                    FROM nodes
                    ORDER BY election_priority, name
                    "#,
                )
                .fetch_all(pool.as_ref())
                .await?;

                rows.into_iter().map(map_postgres_node_row).collect()
            }
        }
    }

    pub async fn insert_task(&self, task: &TaskRow) -> Result<(), DbError> {
        match self {
            Self::Sqlite(pool) => {
                let task = task.clone();
                pool.with_conn(move |conn| queries::insert_task(conn, &task))
                    .await
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    r#"
                    INSERT INTO tasks (
                        id,
                        kind,
                        payload_json,
                        status,
                        assigned_node,
                        priority,
                        created_at,
                        started_at,
                        completed_at
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9
                    )
                    "#,
                )
                .bind(&task.id)
                .bind(&task.kind)
                .bind(&task.payload_json)
                .bind(&task.status)
                .bind(task.assigned_node.as_deref())
                .bind(task.priority)
                .bind(&task.created_at)
                .bind(task.started_at.as_deref())
                .bind(task.completed_at.as_deref())
                .execute(pool.as_ref())
                .await?;
                Ok(())
            }
        }
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Option<TaskRow>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let task_id = task_id.to_string();
                pool.with_conn(move |conn| queries::get_task(conn, &task_id))
                    .await
            }
            Self::Postgres(pool) => {
                let row = sqlx::query(
                    r#"
                    SELECT
                        id,
                        kind,
                        payload_json,
                        status,
                        assigned_node,
                        priority,
                        created_at,
                        started_at,
                        completed_at
                    FROM tasks
                    WHERE id = $1
                    "#,
                )
                .bind(task_id)
                .fetch_optional(pool.as_ref())
                .await?;

                row.map(map_postgres_task_row).transpose()
            }
        }
    }

    pub async fn list_tasks_by_status(&self, status: &str) -> Result<Vec<TaskRow>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let status = status.to_string();
                pool.with_conn(move |conn| queries::list_tasks_by_status(conn, &status))
                    .await
            }
            Self::Postgres(pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        id,
                        kind,
                        payload_json,
                        status,
                        assigned_node,
                        priority,
                        created_at,
                        started_at,
                        completed_at
                    FROM tasks
                    WHERE status = $1
                    ORDER BY priority DESC, created_at
                    "#,
                )
                .bind(status)
                .fetch_all(pool.as_ref())
                .await?;

                rows.into_iter().map(map_postgres_task_row).collect()
            }
        }
    }

    pub async fn claim_next_task(&self, node_name: &str) -> Result<Option<TaskRow>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let node = node_name.to_string();
                pool.with_conn_mut(move |conn| queries::claim_next_task(conn, &node))
                    .await
            }
            Self::Postgres(pool) => {
                let now = now_iso();
                let row = sqlx::query(
                    r#"
                    WITH candidate AS (
                        SELECT id
                        FROM tasks
                        WHERE status IN ('pending', 'todo', 'backlog')
                          AND (assigned_node IS NULL OR assigned_node = '')
                        ORDER BY priority DESC, created_at ASC
                        FOR UPDATE SKIP LOCKED
                        LIMIT 1
                    ),
                    updated AS (
                        UPDATE tasks AS t
                        SET status = 'claimed',
                            assigned_node = $1,
                            started_at = COALESCE(t.started_at, $2)
                        FROM candidate
                        WHERE t.id = candidate.id
                        RETURNING
                            t.id,
                            t.kind,
                            t.payload_json,
                            t.status,
                            t.assigned_node,
                            t.priority,
                            t.created_at,
                            t.started_at,
                            t.completed_at
                    )
                    SELECT * FROM updated
                    "#,
                )
                .bind(node_name)
                .bind(&now)
                .fetch_optional(pool.as_ref())
                .await?;

                row.map(map_postgres_task_row).transpose()
            }
        }
    }

    pub async fn set_task_status(&self, task_id: &str, status: &str) -> Result<bool, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let task_id = task_id.to_string();
                let status = status.to_string();
                pool.with_conn(move |conn| queries::set_task_status(conn, &task_id, &status))
                    .await
            }
            Self::Postgres(pool) => {
                let normalized = status.trim().to_ascii_lowercase();
                let now = now_iso();

                let affected = match normalized.as_str() {
                    "claimed" | "in_progress" | "review" | "running" => sqlx::query(
                        r#"
                            UPDATE tasks
                            SET status = $1,
                                started_at = COALESCE(started_at, $2)
                            WHERE id = $3
                            "#,
                    )
                    .bind(&normalized)
                    .bind(&now)
                    .bind(task_id)
                    .execute(pool.as_ref())
                    .await?
                    .rows_affected(),
                    "done" | "failed" | "completed" | "cancelled" => sqlx::query(
                        r#"
                            UPDATE tasks
                            SET status = $1,
                                completed_at = $2,
                                started_at = COALESCE(started_at, $2)
                            WHERE id = $3
                            "#,
                    )
                    .bind(&normalized)
                    .bind(&now)
                    .bind(task_id)
                    .execute(pool.as_ref())
                    .await?
                    .rows_affected(),
                    _ => sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
                        .bind(&normalized)
                        .bind(task_id)
                        .execute(pool.as_ref())
                        .await?
                        .rows_affected(),
                };

                Ok(affected > 0)
            }
        }
    }

    pub async fn record_task_result(
        &self,
        task_id: &str,
        success: bool,
        output: &str,
        duration_ms: i64,
    ) -> Result<(), DbError> {
        match self {
            Self::Sqlite(pool) => {
                let task_id = task_id.to_string();
                let output = output.to_string();
                pool.with_conn(move |conn| {
                    queries::record_task_result(conn, &task_id, success, &output, duration_ms)
                })
                .await
            }
            Self::Postgres(pool) => {
                let now = now_iso();
                sqlx::query(
                    r#"
                    INSERT INTO task_results (task_id, success, output, duration_ms, completed_at)
                    VALUES ($1, $2, $3, $4, $5)
                    ON CONFLICT(task_id) DO UPDATE SET
                        success = EXCLUDED.success,
                        output = EXCLUDED.output,
                        duration_ms = EXCLUDED.duration_ms,
                        completed_at = EXCLUDED.completed_at
                    "#,
                )
                .bind(task_id)
                .bind(success)
                .bind(output)
                .bind(duration_ms)
                .bind(&now)
                .execute(pool.as_ref())
                .await?;
                Ok(())
            }
        }
    }

    pub async fn ownership_claim(
        &self,
        task_id: &str,
        owner_node: &str,
        lease_secs: i64,
    ) -> Result<bool, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let task_id = task_id.to_string();
                let owner_node = owner_node.to_string();
                pool.with_conn(move |conn| {
                    queries::ownership_claim(conn, &task_id, &owner_node, lease_secs)
                })
                .await
            }
            Self::Postgres(pool) => {
                let now = Utc::now();
                let now_iso = now.to_rfc3339_opts(SecondsFormat::Millis, true);
                let lease_expires_at = (now + Duration::seconds(lease_secs.max(0)))
                    .to_rfc3339_opts(SecondsFormat::Millis, true);

                let changed = sqlx::query(
                    r#"
                    INSERT INTO task_ownership (
                        task_id,
                        owner_node,
                        lease_expires_at,
                        status,
                        handoff_target,
                        updated_at
                    )
                    VALUES ($1, $2, $3, 'claimed', NULL, $4)
                    ON CONFLICT(task_id) DO UPDATE SET
                        owner_node = EXCLUDED.owner_node,
                        lease_expires_at = EXCLUDED.lease_expires_at,
                        status = 'claimed',
                        handoff_target = NULL,
                        updated_at = EXCLUDED.updated_at
                    WHERE task_ownership.owner_node = EXCLUDED.owner_node
                       OR task_ownership.status = 'released'
                       OR task_ownership.lease_expires_at <= EXCLUDED.updated_at
                    "#,
                )
                .bind(task_id)
                .bind(owner_node)
                .bind(&lease_expires_at)
                .bind(&now_iso)
                .execute(pool.as_ref())
                .await?
                .rows_affected();

                Ok(changed > 0)
            }
        }
    }

    pub async fn ownership_release(
        &self,
        task_id: &str,
        owner_node: &str,
    ) -> Result<bool, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let task_id = task_id.to_string();
                let owner_node = owner_node.to_string();
                pool.with_conn(move |conn| queries::ownership_release(conn, &task_id, &owner_node))
                    .await
            }
            Self::Postgres(pool) => {
                let now_iso = now_iso();
                let changed = sqlx::query(
                    r#"
                    UPDATE task_ownership
                    SET status = 'released',
                        handoff_target = NULL,
                        lease_expires_at = $1,
                        updated_at = $1
                    WHERE task_id = $2
                      AND owner_node = $3
                      AND status != 'released'
                    "#,
                )
                .bind(&now_iso)
                .bind(task_id)
                .bind(owner_node)
                .execute(pool.as_ref())
                .await?
                .rows_affected();

                Ok(changed > 0)
            }
        }
    }

    pub async fn audit_log(
        &self,
        event_type: &str,
        actor: &str,
        target: Option<&str>,
        details_json: &str,
        node_name: Option<&str>,
    ) -> Result<i64, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let event_type = event_type.to_string();
                let actor = actor.to_string();
                let target = target.map(ToString::to_string);
                let details_json = details_json.to_string();
                let node_name = node_name.map(ToString::to_string);

                pool.with_conn(move |conn| {
                    queries::audit_log(
                        conn,
                        &event_type,
                        &actor,
                        target.as_deref(),
                        &details_json,
                        node_name.as_deref(),
                    )
                })
                .await
            }
            Self::Postgres(pool) => {
                let row = sqlx::query(
                    r#"
                    INSERT INTO audit_log (
                        timestamp,
                        event_type,
                        actor,
                        target,
                        details_json,
                        node_name
                    )
                    VALUES ($1, $2, $3, $4, $5, $6)
                    RETURNING id
                    "#,
                )
                .bind(now_iso())
                .bind(event_type)
                .bind(actor)
                .bind(target)
                .bind(details_json)
                .bind(node_name)
                .fetch_one(pool.as_ref())
                .await?;

                Ok(row.try_get::<i64, _>("id")?)
            }
        }
    }

    pub async fn recent_audit_log(&self, limit: u32) -> Result<Vec<AuditLogRow>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                pool.with_conn(move |conn| queries::recent_audit_log(conn, limit))
                    .await
            }
            Self::Postgres(pool) => {
                let clamped = limit.clamp(1, 500) as i64;
                let rows = sqlx::query(
                    r#"
                    SELECT
                        id,
                        timestamp,
                        event_type,
                        actor,
                        target,
                        details_json,
                        node_name
                    FROM audit_log
                    ORDER BY id DESC
                    LIMIT $1
                    "#,
                )
                .bind(clamped)
                .fetch_all(pool.as_ref())
                .await?;

                rows.into_iter().map(map_postgres_audit_row).collect()
            }
        }
    }

    pub async fn insert_autonomy_event(
        &self,
        event_type: &str,
        action_type: &str,
        decision: &str,
        reason: &str,
    ) -> Result<i64, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let event_type = event_type.to_string();
                let action_type = action_type.to_string();
                let decision = decision.to_string();
                let reason = reason.to_string();
                pool.with_conn(move |conn| {
                    queries::insert_autonomy_event(
                        conn,
                        &event_type,
                        &action_type,
                        &decision,
                        &reason,
                    )
                })
                .await
            }
            Self::Postgres(pool) => {
                let row = sqlx::query(
                    r#"
                    INSERT INTO autonomy_events (
                        event_type,
                        action_type,
                        decision,
                        reason,
                        created_at
                    )
                    VALUES ($1, $2, $3, $4, $5)
                    RETURNING id
                    "#,
                )
                .bind(event_type)
                .bind(action_type)
                .bind(decision)
                .bind(reason)
                .bind(now_iso())
                .fetch_one(pool.as_ref())
                .await?;
                Ok(row.try_get::<i64, _>("id")?)
            }
        }
    }

    pub async fn list_recent_autonomy_events(
        &self,
        limit: u32,
    ) -> Result<Vec<AutonomyEventRow>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                pool.with_conn(move |conn| queries::list_recent_autonomy_events(conn, limit))
                    .await
            }
            Self::Postgres(pool) => {
                let clamped = limit.clamp(1, 500) as i64;
                let rows = sqlx::query(
                    r#"
                    SELECT id, event_type, action_type, decision, reason, created_at
                    FROM autonomy_events
                    ORDER BY id DESC
                    LIMIT $1
                    "#,
                )
                .bind(clamped)
                .fetch_all(pool.as_ref())
                .await?;

                rows.into_iter().map(map_postgres_autonomy_row).collect()
            }
        }
    }

    pub async fn insert_telegram_media_ingest(
        &self,
        chat_id: &str,
        message_id: &str,
        media_kind: &str,
        local_path: &str,
        mime_type: Option<&str>,
        size_bytes: Option<u64>,
    ) -> Result<i64, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let chat_id = chat_id.to_string();
                let message_id = message_id.to_string();
                let media_kind = media_kind.to_string();
                let local_path = local_path.to_string();
                let mime_type = mime_type.map(ToString::to_string);
                pool.with_conn(move |conn| {
                    queries::insert_telegram_media_ingest(
                        conn,
                        &chat_id,
                        &message_id,
                        &media_kind,
                        &local_path,
                        mime_type.as_deref(),
                        size_bytes,
                    )
                })
                .await
            }
            Self::Postgres(pool) => {
                let row = sqlx::query(
                    r#"
                    INSERT INTO telegram_media_ingest (
                        chat_id,
                        message_id,
                        media_kind,
                        local_path,
                        mime_type,
                        size_bytes,
                        created_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    RETURNING id
                    "#,
                )
                .bind(chat_id)
                .bind(message_id)
                .bind(media_kind)
                .bind(local_path)
                .bind(mime_type)
                .bind(size_bytes.map(|value| value as i64))
                .bind(now_iso())
                .fetch_one(pool.as_ref())
                .await?;
                Ok(row.try_get::<i64, _>("id")?)
            }
        }
    }

    pub async fn config_set(&self, key: &str, value: &str) -> Result<(), DbError> {
        match self {
            Self::Sqlite(pool) => {
                let key = key.to_string();
                let value = value.to_string();
                pool.with_conn(move |conn| queries::config_set(conn, &key, &value))
                    .await
            }
            Self::Postgres(pool) => {
                let now = now_iso();
                sqlx::query(
                    r#"
                    INSERT INTO config_kv (key, value, updated_at)
                    VALUES ($1, $2, $3)
                    ON CONFLICT(key) DO UPDATE SET
                        value = EXCLUDED.value,
                        updated_at = EXCLUDED.updated_at
                    "#,
                )
                .bind(key)
                .bind(value)
                .bind(&now)
                .execute(pool.as_ref())
                .await?;
                Ok(())
            }
        }
    }

    pub async fn config_get(&self, key: &str) -> Result<Option<String>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let key = key.to_string();
                pool.with_conn(move |conn| queries::config_get(conn, &key))
                    .await
            }
            Self::Postgres(pool) => {
                let value =
                    sqlx::query_scalar::<_, String>("SELECT value FROM config_kv WHERE key = $1")
                        .bind(key)
                        .fetch_optional(pool.as_ref())
                        .await?;
                Ok(value)
            }
        }
    }

    pub async fn config_delete(&self, key: &str) -> Result<bool, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let key = key.to_string();
                pool.with_conn(move |conn| {
                    let changed = conn.execute(
                        "DELETE FROM config_kv WHERE key = ?1",
                        rusqlite::params![key],
                    )?;
                    Ok(changed > 0)
                })
                .await
            }
            Self::Postgres(pool) => {
                let changed = sqlx::query("DELETE FROM config_kv WHERE key = $1")
                    .bind(key)
                    .execute(pool.as_ref())
                    .await?
                    .rows_affected();
                Ok(changed > 0)
            }
        }
    }

    pub async fn config_list_prefix(
        &self,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<(String, String)>, DbError> {
        let clamped = limit.clamp(1, 10_000) as i64;
        let like_pattern = format!("{}%", prefix);

        match self {
            Self::Sqlite(pool) => {
                pool.with_conn(move |conn| {
                    let mut stmt = conn.prepare(
                        "SELECT key, value FROM config_kv WHERE key LIKE ?1 ORDER BY key ASC LIMIT ?2",
                    )?;
                    let rows = stmt.query_map(rusqlite::params![like_pattern, clamped], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;

                    let mut collected = Vec::new();
                    for row in rows {
                        collected.push(row?);
                    }
                    Ok(collected)
                })
                .await
            }
            Self::Postgres(pool) => {
                let rows = sqlx::query(
                    "SELECT key, value FROM config_kv WHERE key LIKE $1 ORDER BY key ASC LIMIT $2",
                )
                .bind(&like_pattern)
                .bind(clamped)
                .fetch_all(pool.as_ref())
                .await?;

                let mut collected = Vec::with_capacity(rows.len());
                for row in rows {
                    collected.push((
                        row.try_get::<String, _>("key")?,
                        row.try_get::<String, _>("value")?,
                    ));
                }
                Ok(collected)
            }
        }
    }

    async fn ensure_postgres_schema(&self) -> Result<(), DbError> {
        let Self::Postgres(pool) = self else {
            return Ok(());
        };

        // Core fleet metadata
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS nodes (
                id                TEXT PRIMARY KEY,
                name              TEXT NOT NULL UNIQUE,
                host              TEXT NOT NULL,
                port              BIGINT NOT NULL DEFAULT 55000,
                role              TEXT NOT NULL DEFAULT 'worker',
                election_priority BIGINT NOT NULL DEFAULT 99,
                status            TEXT NOT NULL DEFAULT 'online',
                hardware_json     TEXT NOT NULL DEFAULT '{}',
                models_json       TEXT NOT NULL DEFAULT '[]',
                last_heartbeat    TEXT,
                registered_at     TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS models (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                tier        BIGINT NOT NULL,
                params_b    DOUBLE PRECISION NOT NULL,
                quant       TEXT NOT NULL DEFAULT 'Q4_K_M',
                path        TEXT NOT NULL DEFAULT '',
                ctx_size    BIGINT NOT NULL DEFAULT 8192,
                runtime     TEXT NOT NULL DEFAULT 'llama_cpp',
                nodes_json  TEXT NOT NULL DEFAULT '[]',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        // Tasks / execution
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                id            TEXT PRIMARY KEY,
                kind          TEXT NOT NULL,
                payload_json  TEXT NOT NULL DEFAULT '{}',
                status        TEXT NOT NULL DEFAULT 'pending',
                assigned_node TEXT,
                priority      BIGINT NOT NULL DEFAULT 0,
                created_at    TEXT NOT NULL,
                started_at    TEXT,
                completed_at  TEXT
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS task_results (
                id           BIGSERIAL PRIMARY KEY,
                task_id      TEXT NOT NULL UNIQUE,
                success      BOOLEAN NOT NULL DEFAULT FALSE,
                output       TEXT NOT NULL DEFAULT '',
                duration_ms  BIGINT NOT NULL DEFAULT 0,
                completed_at TEXT NOT NULL,
                CONSTRAINT fk_task_results_task
                    FOREIGN KEY (task_id) REFERENCES tasks(id)
                    ON DELETE CASCADE
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        // Ownership and handoff
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS task_ownership (
                task_id          TEXT PRIMARY KEY,
                owner_node       TEXT NOT NULL,
                lease_expires_at TEXT NOT NULL,
                status           TEXT NOT NULL DEFAULT 'claimed',
                handoff_target   TEXT,
                updated_at       TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS ownership_events (
                id         BIGSERIAL PRIMARY KEY,
                task_id    TEXT NOT NULL,
                event_type TEXT NOT NULL,
                from_owner TEXT,
                to_owner   TEXT,
                reason     TEXT,
                created_at TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        // Agent policy/audit trail
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS autonomy_events (
                id          BIGSERIAL PRIMARY KEY,
                event_type  TEXT NOT NULL,
                action_type TEXT NOT NULL,
                decision    TEXT NOT NULL,
                reason      TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS telegram_media_ingest (
                id         BIGSERIAL PRIMARY KEY,
                chat_id    TEXT NOT NULL,
                message_id TEXT NOT NULL,
                media_kind TEXT NOT NULL,
                local_path TEXT NOT NULL,
                mime_type  TEXT,
                size_bytes BIGINT,
                created_at TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id             TEXT PRIMARY KEY,
                namespace      TEXT NOT NULL DEFAULT 'default',
                key            TEXT NOT NULL,
                content        TEXT NOT NULL,
                embedding_json TEXT,
                metadata_json  TEXT NOT NULL DEFAULT '{}',
                importance     DOUBLE PRECISION NOT NULL DEFAULT 0.5,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL,
                expires_at     TEXT,
                UNIQUE(namespace, key)
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id            TEXT PRIMARY KEY,
                channel       TEXT NOT NULL DEFAULT 'unknown',
                user_id       TEXT,
                node_name     TEXT,
                status        TEXT NOT NULL DEFAULT 'active',
                metadata_json TEXT NOT NULL DEFAULT '{}',
                created_at    TEXT NOT NULL,
                last_activity TEXT NOT NULL,
                closed_at     TEXT
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS cron_jobs (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL UNIQUE,
                schedule      TEXT NOT NULL,
                task_kind     TEXT NOT NULL,
                payload_json  TEXT NOT NULL DEFAULT '{}',
                enabled       BOOLEAN NOT NULL DEFAULT TRUE,
                node_affinity TEXT,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS cron_runs (
                id           BIGSERIAL PRIMARY KEY,
                cron_job_id  TEXT NOT NULL,
                started_at   TEXT NOT NULL,
                completed_at TEXT,
                success      BOOLEAN,
                output       TEXT NOT NULL DEFAULT '',
                duration_ms  BIGINT,
                CONSTRAINT fk_cron_runs_job
                    FOREIGN KEY (cron_job_id) REFERENCES cron_jobs(id)
                    ON DELETE CASCADE
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS audit_log (
                id           BIGSERIAL PRIMARY KEY,
                timestamp    TEXT NOT NULL,
                event_type   TEXT NOT NULL,
                actor        TEXT NOT NULL DEFAULT 'system',
                target       TEXT,
                details_json TEXT NOT NULL DEFAULT '{}',
                node_name    TEXT
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS config_kv (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        // Indexes
        for statement in [
            "CREATE INDEX IF NOT EXISTS idx_tasks_status_priority ON tasks(status, priority DESC, created_at)",
            "CREATE INDEX IF NOT EXISTS idx_task_ownership_owner ON task_ownership(owner_node)",
            "CREATE INDEX IF NOT EXISTS idx_task_ownership_status ON task_ownership(status)",
            "CREATE INDEX IF NOT EXISTS idx_task_ownership_lease ON task_ownership(lease_expires_at)",
            "CREATE INDEX IF NOT EXISTS idx_ownership_events_task ON ownership_events(task_id, id)",
            "CREATE INDEX IF NOT EXISTS idx_autonomy_events_created_at ON autonomy_events(created_at)",
            "CREATE INDEX IF NOT EXISTS idx_tg_media_ingest_chat_created ON telegram_media_ingest(chat_id, created_at)",
            "CREATE INDEX IF NOT EXISTS idx_tg_media_ingest_message ON telegram_media_ingest(message_id)",
            "CREATE INDEX IF NOT EXISTS idx_sessions_channel ON sessions(channel)",
            "CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status)",
            "CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace)",
            "CREATE INDEX IF NOT EXISTS idx_memories_importance ON memories(importance DESC)",
            "CREATE INDEX IF NOT EXISTS idx_cron_runs_job ON cron_runs(cron_job_id)",
            "CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp)",
            "CREATE INDEX IF NOT EXISTS idx_audit_event_type ON audit_log(event_type)",
        ] {
            sqlx::query(statement).execute(pool.as_ref()).await?;
        }

        info!("postgres operational schema ready");
        Ok(())
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn map_postgres_node_row(row: sqlx::postgres::PgRow) -> Result<NodeRow, DbError> {
    Ok(NodeRow {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        host: row.try_get("host")?,
        port: row.try_get("port")?,
        role: row.try_get("role")?,
        election_priority: row.try_get("election_priority")?,
        status: row.try_get("status")?,
        hardware_json: row.try_get("hardware_json")?,
        models_json: row.try_get("models_json")?,
        last_heartbeat: row.try_get("last_heartbeat")?,
        registered_at: row.try_get("registered_at")?,
    })
}

fn map_postgres_task_row(row: sqlx::postgres::PgRow) -> Result<TaskRow, DbError> {
    Ok(TaskRow {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        payload_json: row.try_get("payload_json")?,
        status: row.try_get("status")?,
        assigned_node: row.try_get("assigned_node")?,
        priority: row.try_get("priority")?,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        completed_at: row.try_get("completed_at")?,
    })
}

fn map_postgres_audit_row(row: sqlx::postgres::PgRow) -> Result<AuditLogRow, DbError> {
    Ok(AuditLogRow {
        id: row.try_get("id")?,
        timestamp: row.try_get("timestamp")?,
        event_type: row.try_get("event_type")?,
        actor: row.try_get("actor")?,
        target: row.try_get("target")?,
        details_json: row.try_get("details_json")?,
        node_name: row.try_get("node_name")?,
    })
}

fn map_postgres_autonomy_row(row: sqlx::postgres::PgRow) -> Result<AutonomyEventRow, DbError> {
    Ok(AutonomyEventRow {
        id: row.try_get("id")?,
        event_type: row.try_get("event_type")?,
        action_type: row.try_get("action_type")?,
        decision: row.try_get("decision")?,
        reason: row.try_get("reason")?,
        created_at: row.try_get("created_at")?,
    })
}
