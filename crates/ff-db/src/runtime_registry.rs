//! Runtime registry persistence abstraction.
//!
//! Phase 37A transitional model:
//! - Keep full embedded SQLite support.
//! - Allow runtime registry + enrollment event tables to be primary on Postgres.
//!
//! This module intentionally scopes Postgres writes to operational runtime tables:
//! `fleet_node_runtime` and `fleet_enrollment_events`.

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tracing::info;

use crate::{DbPool, error::DbError, queries};

/// Persistence backend for runtime registry/enrollment tables.
#[derive(Debug, Clone)]
pub enum RuntimeRegistryStore {
    /// Persist runtime registry rows into embedded SQLite.
    Sqlite(DbPool),
    /// Persist runtime registry rows into Postgres.
    Postgres(Arc<PgPool>),
}

impl RuntimeRegistryStore {
    /// Build runtime registry store backed by SQLite.
    pub fn sqlite(pool: DbPool) -> Self {
        Self::Sqlite(pool)
    }

    /// Build runtime registry store backed by Postgres and ensure schema exists.
    pub async fn postgres(database_url: &str, max_connections: u32) -> Result<Self, DbError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .connect(database_url)
            .await?;

        let store = Self::Postgres(Arc::new(pool));
        store.ensure_postgres_schema().await?;
        Ok(store)
    }

    /// Human-readable backend label for logs and diagnostics.
    pub fn backend_label(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "embedded_sqlite",
            Self::Postgres(_) => "postgres_runtime",
        }
    }

    /// Persist heartbeat row and return the stored runtime node view.
    pub async fn upsert_runtime(
        &self,
        heartbeat: &queries::FleetNodeRuntimeHeartbeatRow,
    ) -> Result<queries::FleetNodeRuntimeRow, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let heartbeat = heartbeat.clone();
                let node_id = heartbeat.node_id.clone();
                let row = pool
                    .with_conn(move |conn| {
                        queries::upsert_fleet_node_runtime(conn, &heartbeat)?;
                        let rows = queries::list_fleet_node_runtime(conn)?;
                        Ok(rows.into_iter().find(|row| row.node_id == node_id))
                    })
                    .await?;

                row.ok_or_else(|| DbError::NotFound("runtime node readback failed".to_string()))
            }
            Self::Postgres(pool) => {
                let now = Utc::now();
                let heartbeat_at = parse_rfc3339_to_utc(&heartbeat.last_heartbeat)?;

                sqlx::query(
                    r#"
                    INSERT INTO fleet_node_runtime (
                        node_id,
                        hostname,
                        ips_json,
                        role,
                        reported_status,
                        last_heartbeat,
                        resources_json,
                        services_json,
                        models_json,
                        capabilities_json,
                        stale_degraded_after_secs,
                        stale_offline_after_secs,
                        updated_at
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13
                    )
                    ON CONFLICT (node_id) DO UPDATE SET
                        hostname = EXCLUDED.hostname,
                        ips_json = EXCLUDED.ips_json,
                        role = EXCLUDED.role,
                        reported_status = EXCLUDED.reported_status,
                        last_heartbeat = EXCLUDED.last_heartbeat,
                        resources_json = EXCLUDED.resources_json,
                        services_json = EXCLUDED.services_json,
                        models_json = EXCLUDED.models_json,
                        capabilities_json = EXCLUDED.capabilities_json,
                        stale_degraded_after_secs = EXCLUDED.stale_degraded_after_secs,
                        stale_offline_after_secs = EXCLUDED.stale_offline_after_secs,
                        updated_at = EXCLUDED.updated_at
                    "#,
                )
                .bind(&heartbeat.node_id)
                .bind(&heartbeat.hostname)
                .bind(&heartbeat.ips_json)
                .bind(&heartbeat.role)
                .bind(&heartbeat.reported_status)
                .bind(heartbeat_at)
                .bind(&heartbeat.resources_json)
                .bind(&heartbeat.services_json)
                .bind(&heartbeat.models_json)
                .bind(&heartbeat.capabilities_json)
                .bind(heartbeat.stale_degraded_after_secs.max(1))
                .bind(
                    heartbeat
                        .stale_offline_after_secs
                        .max(heartbeat.stale_degraded_after_secs.max(1) + 1),
                )
                .bind(now)
                .execute(pool.as_ref())
                .await?;

                self.get_runtime_node_by_id(&heartbeat.node_id).await
            }
        }
    }

    /// Persist accepted enrollment event + runtime row atomically where possible.
    pub async fn upsert_runtime_with_enrollment(
        &self,
        heartbeat: &queries::FleetNodeRuntimeHeartbeatRow,
        event: &queries::FleetEnrollmentEventInsert,
    ) -> Result<queries::FleetNodeRuntimeRow, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let heartbeat = heartbeat.clone();
                let event = event.clone();
                let node_id = heartbeat.node_id.clone();
                let row = pool
                    .with_conn(move |conn| {
                        queries::upsert_fleet_node_runtime(conn, &heartbeat)?;
                        queries::insert_fleet_enrollment_event(conn, &event)?;
                        let rows = queries::list_fleet_node_runtime(conn)?;
                        Ok(rows.into_iter().find(|row| row.node_id == node_id))
                    })
                    .await?;

                row.ok_or_else(|| DbError::NotFound("runtime node readback failed".to_string()))
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let now = Utc::now();
                let heartbeat_at = parse_rfc3339_to_utc(&heartbeat.last_heartbeat)?;

                sqlx::query(
                    r#"
                    INSERT INTO fleet_node_runtime (
                        node_id,
                        hostname,
                        ips_json,
                        role,
                        reported_status,
                        last_heartbeat,
                        resources_json,
                        services_json,
                        models_json,
                        capabilities_json,
                        stale_degraded_after_secs,
                        stale_offline_after_secs,
                        updated_at
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13
                    )
                    ON CONFLICT (node_id) DO UPDATE SET
                        hostname = EXCLUDED.hostname,
                        ips_json = EXCLUDED.ips_json,
                        role = EXCLUDED.role,
                        reported_status = EXCLUDED.reported_status,
                        last_heartbeat = EXCLUDED.last_heartbeat,
                        resources_json = EXCLUDED.resources_json,
                        services_json = EXCLUDED.services_json,
                        models_json = EXCLUDED.models_json,
                        capabilities_json = EXCLUDED.capabilities_json,
                        stale_degraded_after_secs = EXCLUDED.stale_degraded_after_secs,
                        stale_offline_after_secs = EXCLUDED.stale_offline_after_secs,
                        updated_at = EXCLUDED.updated_at
                    "#,
                )
                .bind(&heartbeat.node_id)
                .bind(&heartbeat.hostname)
                .bind(&heartbeat.ips_json)
                .bind(&heartbeat.role)
                .bind(&heartbeat.reported_status)
                .bind(heartbeat_at)
                .bind(&heartbeat.resources_json)
                .bind(&heartbeat.services_json)
                .bind(&heartbeat.models_json)
                .bind(&heartbeat.capabilities_json)
                .bind(heartbeat.stale_degraded_after_secs.max(1))
                .bind(
                    heartbeat
                        .stale_offline_after_secs
                        .max(heartbeat.stale_degraded_after_secs.max(1) + 1),
                )
                .bind(now)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    INSERT INTO fleet_enrollment_events (
                        node_id,
                        hostname,
                        outcome,
                        reason,
                        role,
                        service_version,
                        addresses_json,
                        capabilities_json,
                        metadata_json
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9
                    )
                    "#,
                )
                .bind(event.node_id.as_deref())
                .bind(event.hostname.as_deref())
                .bind(&event.outcome)
                .bind(event.reason.as_deref())
                .bind(event.role.as_deref())
                .bind(event.service_version.as_deref())
                .bind(&event.addresses_json)
                .bind(&event.capabilities_json)
                .bind(&event.metadata_json)
                .execute(&mut *tx)
                .await?;

                let row = sqlx::query(
                    r#"
                    SELECT
                        node_id,
                        hostname,
                        ips_json,
                        role,
                        reported_status,
                        last_heartbeat,
                        resources_json,
                        services_json,
                        models_json,
                        capabilities_json,
                        stale_degraded_after_secs,
                        stale_offline_after_secs,
                        updated_at
                    FROM fleet_node_runtime
                    WHERE node_id = $1
                    "#,
                )
                .bind(&heartbeat.node_id)
                .fetch_optional(&mut *tx)
                .await?;

                tx.commit().await?;

                let row = row.ok_or_else(|| {
                    DbError::NotFound("runtime node readback failed after enrollment".to_string())
                })?;

                map_postgres_runtime_row(row)
            }
        }
    }

    /// Persist enrollment event and return new row id.
    pub async fn insert_enrollment_event(
        &self,
        event: &queries::FleetEnrollmentEventInsert,
    ) -> Result<i64, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let event = event.clone();
                pool.with_conn(move |conn| queries::insert_fleet_enrollment_event(conn, &event))
                    .await
            }
            Self::Postgres(pool) => {
                let row = sqlx::query(
                    r#"
                    INSERT INTO fleet_enrollment_events (
                        node_id,
                        hostname,
                        outcome,
                        reason,
                        role,
                        service_version,
                        addresses_json,
                        capabilities_json,
                        metadata_json
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9
                    )
                    RETURNING id
                    "#,
                )
                .bind(event.node_id.as_deref())
                .bind(event.hostname.as_deref())
                .bind(&event.outcome)
                .bind(event.reason.as_deref())
                .bind(event.role.as_deref())
                .bind(event.service_version.as_deref())
                .bind(&event.addresses_json)
                .bind(&event.capabilities_json)
                .bind(&event.metadata_json)
                .fetch_one(pool.as_ref())
                .await?;

                Ok(row.try_get::<i64, _>("id")?)
            }
        }
    }

    /// List runtime nodes.
    pub async fn list_runtime_nodes(&self) -> Result<Vec<queries::FleetNodeRuntimeRow>, DbError> {
        match self {
            Self::Sqlite(pool) => pool.with_conn(queries::list_fleet_node_runtime).await,
            Self::Postgres(pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        node_id,
                        hostname,
                        ips_json,
                        role,
                        reported_status,
                        last_heartbeat,
                        resources_json,
                        services_json,
                        models_json,
                        capabilities_json,
                        stale_degraded_after_secs,
                        stale_offline_after_secs,
                        updated_at
                    FROM fleet_node_runtime
                    ORDER BY hostname, node_id
                    "#,
                )
                .fetch_all(pool.as_ref())
                .await?;

                rows.into_iter().map(map_postgres_runtime_row).collect()
            }
        }
    }

    /// List enrollment events.
    pub async fn list_enrollment_events(
        &self,
        limit: usize,
    ) -> Result<Vec<queries::FleetEnrollmentEventRow>, DbError> {
        match self {
            Self::Sqlite(pool) => {
                pool.with_conn(move |conn| queries::list_fleet_enrollment_events(conn, limit))
                    .await
            }
            Self::Postgres(pool) => {
                let clamped = (limit as i64).clamp(1, 500);
                let rows = sqlx::query(
                    r#"
                    SELECT
                        id,
                        node_id,
                        hostname,
                        outcome,
                        reason,
                        role,
                        service_version,
                        addresses_json,
                        capabilities_json,
                        metadata_json,
                        created_at
                    FROM fleet_enrollment_events
                    ORDER BY id DESC
                    LIMIT $1
                    "#,
                )
                .bind(clamped)
                .fetch_all(pool.as_ref())
                .await?;

                rows.into_iter().map(map_postgres_enrollment_row).collect()
            }
        }
    }

    async fn get_runtime_node_by_id(
        &self,
        node_id: &str,
    ) -> Result<queries::FleetNodeRuntimeRow, DbError> {
        match self {
            Self::Sqlite(pool) => {
                let node_id = node_id.to_string();
                let row = pool
                    .with_conn(move |conn| {
                        let rows = queries::list_fleet_node_runtime(conn)?;
                        Ok(rows.into_iter().find(|row| row.node_id == node_id))
                    })
                    .await?;

                row.ok_or_else(|| DbError::NotFound("runtime node readback failed".to_string()))
            }
            Self::Postgres(pool) => {
                let row = sqlx::query(
                    r#"
                    SELECT
                        node_id,
                        hostname,
                        ips_json,
                        role,
                        reported_status,
                        last_heartbeat,
                        resources_json,
                        services_json,
                        models_json,
                        capabilities_json,
                        stale_degraded_after_secs,
                        stale_offline_after_secs,
                        updated_at
                    FROM fleet_node_runtime
                    WHERE node_id = $1
                    "#,
                )
                .bind(node_id)
                .fetch_optional(pool.as_ref())
                .await?;

                let row = row.ok_or_else(|| {
                    DbError::NotFound(format!("runtime node '{node_id}' not found"))
                })?;
                map_postgres_runtime_row(row)
            }
        }
    }

    async fn ensure_postgres_schema(&self) -> Result<(), DbError> {
        let Self::Postgres(pool) = self else {
            return Ok(());
        };

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS fleet_node_runtime (
                node_id                    TEXT PRIMARY KEY,
                hostname                   TEXT NOT NULL,
                ips_json                   TEXT NOT NULL DEFAULT '[]',
                role                       TEXT NOT NULL DEFAULT 'worker',
                reported_status            TEXT NOT NULL DEFAULT 'online',
                last_heartbeat             TIMESTAMPTZ NOT NULL,
                resources_json             TEXT NOT NULL DEFAULT '{}',
                services_json              TEXT NOT NULL DEFAULT '[]',
                models_json                TEXT NOT NULL DEFAULT '[]',
                capabilities_json          TEXT NOT NULL DEFAULT '{}',
                stale_degraded_after_secs  BIGINT NOT NULL DEFAULT 90,
                stale_offline_after_secs   BIGINT NOT NULL DEFAULT 180,
                updated_at                 TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_fleet_runtime_hostname ON fleet_node_runtime(hostname)",
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_fleet_runtime_heartbeat ON fleet_node_runtime(last_heartbeat)",
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS fleet_enrollment_events (
                id                BIGSERIAL PRIMARY KEY,
                node_id           TEXT,
                hostname          TEXT,
                outcome           TEXT NOT NULL,
                reason            TEXT,
                role              TEXT,
                service_version   TEXT,
                addresses_json    TEXT NOT NULL DEFAULT '[]',
                capabilities_json TEXT NOT NULL DEFAULT '{}',
                metadata_json     TEXT NOT NULL DEFAULT '{}',
                created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_fleet_enrollment_events_created ON fleet_enrollment_events(created_at DESC)",
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_fleet_enrollment_events_node ON fleet_enrollment_events(node_id, created_at DESC)",
        )
        .execute(pool.as_ref())
        .await?;

        info!("postgres runtime registry schema ready");
        Ok(())
    }
}

fn parse_rfc3339_to_utc(raw: &str) -> Result<DateTime<Utc>, DbError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| DbError::Migration(format!("invalid RFC3339 timestamp '{raw}': {error}")))
}

fn to_iso_millis(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn map_postgres_runtime_row(
    row: sqlx::postgres::PgRow,
) -> Result<queries::FleetNodeRuntimeRow, DbError> {
    let reported_status: String = row.try_get("reported_status")?;
    let last_heartbeat: DateTime<Utc> = row.try_get("last_heartbeat")?;
    let degraded_after_secs: i64 = row.try_get("stale_degraded_after_secs")?;
    let offline_after_secs: i64 = row.try_get("stale_offline_after_secs")?;

    let now = Utc::now();
    let heartbeat_raw = to_iso_millis(last_heartbeat);
    let (derived_status, heartbeat_age_secs) = queries::derive_runtime_node_status(
        &reported_status,
        &heartbeat_raw,
        &now,
        degraded_after_secs,
        offline_after_secs,
    );

    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;

    Ok(queries::FleetNodeRuntimeRow {
        node_id: row.try_get("node_id")?,
        hostname: row.try_get("hostname")?,
        ips_json: row.try_get("ips_json")?,
        role: row.try_get("role")?,
        reported_status,
        derived_status,
        last_heartbeat: heartbeat_raw,
        heartbeat_age_secs,
        resources_json: row.try_get("resources_json")?,
        services_json: row.try_get("services_json")?,
        models_json: row.try_get("models_json")?,
        capabilities_json: row.try_get("capabilities_json")?,
        stale_degraded_after_secs: degraded_after_secs,
        stale_offline_after_secs: offline_after_secs,
        updated_at: to_iso_millis(updated_at),
    })
}

fn map_postgres_enrollment_row(
    row: sqlx::postgres::PgRow,
) -> Result<queries::FleetEnrollmentEventRow, DbError> {
    let created_at: DateTime<Utc> = row.try_get("created_at")?;

    Ok(queries::FleetEnrollmentEventRow {
        id: row.try_get("id")?,
        node_id: row.try_get("node_id")?,
        hostname: row.try_get("hostname")?,
        outcome: row.try_get("outcome")?,
        reason: row.try_get("reason")?,
        role: row.try_get("role")?,
        service_version: row.try_get("service_version")?,
        addresses_json: row.try_get("addresses_json")?,
        capabilities_json: row.try_get("capabilities_json")?,
        metadata_json: row.try_get("metadata_json")?,
        created_at: to_iso_millis(created_at),
    })
}
