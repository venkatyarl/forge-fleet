//! Embedded migration runner.
//!
//! Migrations are SQL strings embedded in Rust, applied forward-only
//! with version tracking via a `_migrations` meta-table.

use rusqlite::Connection;
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::error::{DbError, Result};
use crate::schema;

/// A single migration step.
struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// All migrations in order. Add new ones at the end — never modify existing entries.
static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        sql: schema::SCHEMA_V1,
    },
    Migration {
        version: 2,
        name: "task_ownership_schema",
        sql: schema::SCHEMA_V2_TASK_OWNERSHIP,
    },
    Migration {
        version: 3,
        name: "autonomy_events_schema",
        sql: schema::SCHEMA_V3_AUTONOMY_EVENTS,
    },
    Migration {
        version: 4,
        name: "telegram_media_ingest_schema",
        sql: schema::SCHEMA_V4_TELEGRAM_MEDIA_INGEST,
    },
    Migration {
        version: 5,
        name: "fleet_node_runtime_schema",
        sql: schema::SCHEMA_V5_FLEET_NODE_RUNTIME,
    },
    Migration {
        version: 6,
        name: "fleet_enrollment_events_schema",
        sql: schema::SCHEMA_V6_FLEET_ENROLLMENT_EVENTS,
    },
];

/// Ensure the migrations meta-table exists.
fn ensure_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version     INTEGER PRIMARY KEY,
            name        TEXT NOT NULL,
            applied_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );",
    )?;
    Ok(())
}

/// Get the current schema version (0 if no migrations have been applied).
fn current_version(conn: &Connection) -> Result<u32> {
    let version: u32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _migrations",
        [],
        |row| row.get(0),
    )?;
    Ok(version)
}

/// Run all pending migrations on the given connection.
///
/// This function is idempotent — re-running it on an up-to-date database is a no-op.
/// Migrations run in a transaction so partial failures roll back cleanly.
pub fn run_migrations(conn: &Connection) -> Result<u32> {
    ensure_migrations_table(conn)?;
    let current = current_version(conn)?;

    let pending: Vec<&Migration> = MIGRATIONS.iter().filter(|m| m.version > current).collect();

    if pending.is_empty() {
        debug!(current_version = current, "database is up to date");
        return Ok(current);
    }

    info!(
        current_version = current,
        pending = pending.len(),
        "running {} pending migration(s)",
        pending.len()
    );

    for migration in &pending {
        info!(
            version = migration.version,
            name = migration.name,
            "applying migration"
        );

        // Wrap each migration in a transaction.
        conn.execute_batch("BEGIN IMMEDIATE;")?;

        match conn.execute_batch(migration.sql) {
            Ok(()) => {
                conn.execute(
                    "INSERT INTO _migrations (version, name) VALUES (?1, ?2)",
                    rusqlite::params![migration.version, migration.name],
                )?;
                conn.execute_batch("COMMIT;")?;
                info!(
                    version = migration.version,
                    "migration applied successfully"
                );
            }
            Err(e) => {
                warn!(version = migration.version, error = %e, "migration failed, rolling back");
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(DbError::Migration(format!(
                    "migration v{} '{}' failed: {e}",
                    migration.version, migration.name
                )));
            }
        }
    }

    let final_version = current_version(conn)?;
    info!(version = final_version, "all migrations applied");
    Ok(final_version)
}

/// Get information about applied migrations.
pub fn applied_migrations(conn: &Connection) -> Result<Vec<(u32, String, String)>> {
    ensure_migrations_table(conn)?;

    let mut stmt =
        conn.prepare("SELECT version, name, applied_at FROM _migrations ORDER BY version")?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, u32>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Get the latest migration version available (not yet applied necessarily).
pub fn latest_available_version() -> u32 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

// ─── Postgres Migrations ─────────────────────────────────────────────────────

/// A single Postgres migration step.
struct PgMigration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// Postgres-only migrations. These run independently from the SQLite migrations
/// above and use their own version sequence.
static PG_MIGRATIONS: &[PgMigration] = &[
    PgMigration {
        version: 7,
        name: "fleet_config_tables",
        sql: schema::SCHEMA_V7_FLEET_POSTGRES,
    },
    PgMigration {
        version: 8,
        name: "task_provenance_schema",
        sql: schema::SCHEMA_V8_TASK_PROVENANCE,
    },
    PgMigration {
        version: 9,
        name: "fleet_secrets",
        sql: schema::SCHEMA_V9_FLEET_SECRETS,
    },
    PgMigration {
        version: 10,
        name: "deferred_tasks",
        sql: schema::SCHEMA_V10_DEFERRED_TASKS,
    },
    PgMigration {
        version: 11,
        name: "model_lifecycle",
        sql: schema::SCHEMA_V11_MODEL_LIFECYCLE,
    },
    PgMigration {
        version: 12,
        name: "onboarding_foundation",
        sql: schema::SCHEMA_V12_ONBOARDING,
    },
    PgMigration {
        version: 13,
        name: "virtual_brain",
        sql: schema::SCHEMA_V13_VIRTUAL_BRAIN,
    },
    PgMigration {
        version: 14,
        name: "computers_and_portfolio",
        sql: schema::SCHEMA_V14_COMPUTERS_AND_PORTFOLIO,
    },
    PgMigration {
        version: 15,
        name: "project_management",
        sql: schema::SCHEMA_V15_PROJECT_MANAGEMENT,
    },
    PgMigration {
        version: 16,
        name: "observability",
        sql: schema::SCHEMA_V16_OBSERVABILITY,
    },
    PgMigration {
        version: 17,
        name: "security_hardening",
        sql: schema::SCHEMA_V17_SECURITY_HARDENING,
    },
    PgMigration {
        version: 18,
        name: "network_scope",
        sql: schema::SCHEMA_V18_NETWORK_SCOPE,
    },
    PgMigration {
        version: 19,
        name: "storage_power_training",
        sql: schema::SCHEMA_V19_STORAGE_POWER_TRAINING,
    },
    PgMigration {
        version: 20,
        name: "port_registry",
        sql: schema::SCHEMA_V20_PORT_REGISTRY,
    },
    PgMigration {
        version: 21,
        name: "drop_deployment_model_fk",
        sql: schema::SCHEMA_V21_DROP_DEPLOYMENT_FK,
    },
    PgMigration {
        version: 22,
        name: "drop_model_presence_fk",
        sql: schema::SCHEMA_V22_DROP_MODEL_PRESENCE_FK,
    },
    PgMigration {
        version: 23,
        name: "sub_agents",
        sql: schema::SCHEMA_V23_SUB_AGENTS,
    },
    PgMigration {
        version: 24,
        name: "external_tools",
        sql: schema::SCHEMA_V24_EXTERNAL_TOOLS,
    },
    PgMigration {
        version: 25,
        name: "social_media_ingest",
        sql: schema::SCHEMA_V25_SOCIAL_MEDIA_INGEST,
    },
    PgMigration {
        version: 26,
        name: "cloud_llm_providers",
        sql: schema::SCHEMA_V26_CLOUD_LLM_PROVIDERS,
    },
    PgMigration {
        version: 27,
        name: "pool_aliases",
        sql: schema::SCHEMA_V27_POOL_ALIASES,
    },
    PgMigration {
        version: 28,
        name: "software_registry_seed",
        sql: schema::SCHEMA_V28_SOFTWARE_REGISTRY_SEED,
    },
    PgMigration {
        version: 29,
        name: "fix_ff_git_linux_playbook",
        sql: schema::SCHEMA_V29_FIX_FF_GIT_LINUX_PLAYBOOK,
    },
    PgMigration {
        version: 30,
        name: "playbook_self_heal_repo",
        sql: schema::SCHEMA_V30_PLAYBOOK_SELF_HEAL_REPO,
    },
    PgMigration {
        version: 31,
        name: "source_tree_path",
        sql: schema::SCHEMA_V31_SOURCE_TREE_PATH,
    },
    PgMigration {
        version: 32,
        name: "playbook_bugfixes",
        sql: schema::SCHEMA_V32_PLAYBOOK_BUGFIXES,
    },
    PgMigration {
        version: 33,
        name: "cli_aliases",
        sql: schema::SCHEMA_V33_CLI_ALIASES,
    },
    PgMigration {
        version: 34,
        name: "retire_alert_policies_toml",
        sql: schema::SCHEMA_V34_RETIRE_ALERT_POLICIES_TOML,
    },
    PgMigration {
        version: 35,
        name: "retire_cloud_llm_providers_toml",
        sql: schema::SCHEMA_V35_RETIRE_CLOUD_LLM_PROVIDERS_TOML,
    },
    PgMigration {
        version: 36,
        name: "retire_task_coverage_toml",
        sql: schema::SCHEMA_V36_RETIRE_TASK_COVERAGE_TOML,
    },
];

/// Ensure the Postgres `_migrations` tracking table exists.
async fn ensure_pg_migrations_table(pool: &PgPool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version     INTEGER PRIMARY KEY,
            name        TEXT NOT NULL,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Get the current Postgres schema version (0 if no migrations applied).
async fn pg_current_version(pool: &PgPool) -> Result<u32> {
    let row: (i32,) = sqlx::query_as("SELECT COALESCE(MAX(version), 0) FROM _migrations")
        .fetch_one(pool)
        .await?;
    Ok(row.0 as u32)
}

/// Run all pending Postgres migrations.
///
/// Idempotent — re-running on an up-to-date database is a no-op.
pub async fn run_postgres_migrations(pool: &PgPool) -> Result<u32> {
    ensure_pg_migrations_table(pool).await?;
    let current = pg_current_version(pool).await?;

    let pending: Vec<&PgMigration> = PG_MIGRATIONS
        .iter()
        .filter(|m| m.version > current)
        .collect();

    if pending.is_empty() {
        debug!(current_version = current, "postgres database is up to date");
        return Ok(current);
    }

    info!(
        current_version = current,
        pending = pending.len(),
        "running {} pending postgres migration(s)",
        pending.len()
    );

    for migration in &pending {
        info!(
            version = migration.version,
            name = migration.name,
            "applying postgres migration"
        );

        // Run DDL via raw_sql (supports multi-statement), then record version.
        let mut tx = pool.begin().await?;

        match sqlx::raw_sql(migration.sql).execute(&mut *tx).await {
            Ok(_) => {
                sqlx::query("INSERT INTO _migrations (version, name) VALUES ($1, $2)")
                    .bind(migration.version as i32)
                    .bind(migration.name)
                    .execute(&mut *tx)
                    .await?;

                tx.commit().await?;
                info!(
                    version = migration.version,
                    "postgres migration applied successfully"
                );
            }
            Err(e) => {
                // Transaction is dropped (rolled back) on error.
                warn!(version = migration.version, error = %e, "postgres migration failed");
                return Err(DbError::Migration(format!(
                    "postgres migration v{} '{}' failed: {e}",
                    migration.version, migration.name
                )));
            }
        }
    }

    let final_version = pg_current_version(pool).await?;
    info!(version = final_version, "all postgres migrations applied");
    Ok(final_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_migrations_run_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        // First run applies all.
        let v1 = run_migrations(&conn).unwrap();
        assert_eq!(v1, latest_available_version());

        // Second run is a no-op.
        let v2 = run_migrations(&conn).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn test_applied_migrations_list() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let applied = applied_migrations(&conn).unwrap();
        assert!(!applied.is_empty());
        assert_eq!(applied[0].0, 1);
        assert_eq!(applied[0].1, "initial_schema");
    }

    #[test]
    fn test_tables_created() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Verify every expected table exists.
        for table in crate::schema::TABLES {
            let count: u32 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table '{}' should exist", table);
        }
    }
}
