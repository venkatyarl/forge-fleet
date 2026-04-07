//! SQLite database management for Mission Control.
//!
//! Provides a thread-safe connection wrapper and schema migrations.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tracing::info;

/// Thread-safe SQLite connection for Mission Control.
#[derive(Debug, Clone)]
pub struct McDb {
    conn: Arc<Mutex<Connection>>,
}

impl McDb {
    /// Open (or create) a SQLite database at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Create an in-memory database (for tests).
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Run all schema migrations.
    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS work_items (
                id             TEXT PRIMARY KEY,
                title          TEXT NOT NULL,
                description    TEXT NOT NULL DEFAULT '',
                status         TEXT NOT NULL DEFAULT 'backlog',
                priority       INTEGER NOT NULL DEFAULT 3,
                assignee       TEXT NOT NULL DEFAULT 'unassigned',
                epic_id        TEXT,
                sprint_id      TEXT,
                task_group_id  TEXT,
                sequence_order INTEGER,
                labels         TEXT NOT NULL DEFAULT '[]',
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS epics (
                id          TEXT PRIMARY KEY,
                title       TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'open',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sprints (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                start_date  TEXT,
                end_date    TEXT,
                goal        TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS review_items (
                id            TEXT PRIMARY KEY,
                work_item_id  TEXT NOT NULL,
                title         TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'pending',
                reviewer      TEXT,
                notes         TEXT,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL,
                FOREIGN KEY(work_item_id) REFERENCES work_items(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS work_item_dependencies (
                work_item_id   TEXT NOT NULL,
                depends_on_id  TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                PRIMARY KEY (work_item_id, depends_on_id),
                FOREIGN KEY(work_item_id) REFERENCES work_items(id) ON DELETE CASCADE,
                FOREIGN KEY(depends_on_id) REFERENCES work_items(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS task_groups (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS legal_entities (
                id                  TEXT PRIMARY KEY,
                name                TEXT NOT NULL,
                entity_type         TEXT NOT NULL,
                jurisdiction        TEXT NOT NULL,
                registration_number TEXT,
                status              TEXT NOT NULL DEFAULT 'active',
                created_at          TEXT NOT NULL,
                updated_at          TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS compliance_obligations (
                id           TEXT PRIMARY KEY,
                entity_id    TEXT NOT NULL,
                title        TEXT NOT NULL,
                description  TEXT NOT NULL DEFAULT '',
                jurisdiction TEXT NOT NULL,
                frequency    TEXT NOT NULL DEFAULT 'annual',
                status       TEXT NOT NULL DEFAULT 'active',
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL,
                FOREIGN KEY(entity_id) REFERENCES legal_entities(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS filings (
                id            TEXT PRIMARY KEY,
                entity_id     TEXT NOT NULL,
                obligation_id TEXT,
                jurisdiction  TEXT NOT NULL,
                due_date      TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'pending',
                filed_on      TEXT,
                notes         TEXT,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL,
                FOREIGN KEY(entity_id) REFERENCES legal_entities(id) ON DELETE CASCADE,
                FOREIGN KEY(obligation_id) REFERENCES compliance_obligations(id) ON DELETE SET NULL
            );

            CREATE TABLE IF NOT EXISTS companies (
                id                     TEXT PRIMARY KEY,
                name                   TEXT NOT NULL,
                business_unit          TEXT,
                status                 TEXT NOT NULL DEFAULT 'active',
                priority               INTEGER NOT NULL DEFAULT 3,
                owner                  TEXT NOT NULL DEFAULT 'unassigned',
                operating_stage        TEXT NOT NULL DEFAULT 'build',
                compliance_sensitivity TEXT NOT NULL DEFAULT 'moderate',
                revenue_model_tags     TEXT NOT NULL DEFAULT '[]',
                created_at             TEXT NOT NULL,
                updated_at             TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS projects (
                id                     TEXT PRIMARY KEY,
                company_id             TEXT NOT NULL,
                name                   TEXT NOT NULL,
                description            TEXT NOT NULL DEFAULT '',
                status                 TEXT NOT NULL DEFAULT 'active',
                priority               INTEGER NOT NULL DEFAULT 3,
                owner                  TEXT NOT NULL DEFAULT 'unassigned',
                operating_stage        TEXT NOT NULL DEFAULT 'build',
                compliance_sensitivity TEXT NOT NULL DEFAULT 'moderate',
                revenue_model_tags     TEXT NOT NULL DEFAULT '[]',
                created_at             TEXT NOT NULL,
                updated_at             TEXT NOT NULL,
                FOREIGN KEY(company_id) REFERENCES companies(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS project_repos (
                id             TEXT PRIMARY KEY,
                project_id     TEXT NOT NULL,
                repository_url TEXT NOT NULL,
                provider       TEXT NOT NULL DEFAULT 'github',
                default_branch TEXT NOT NULL DEFAULT 'main',
                status         TEXT NOT NULL DEFAULT 'active',
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL,
                FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS project_environments (
                id               TEXT PRIMARY KEY,
                project_id       TEXT NOT NULL,
                name             TEXT NOT NULL,
                environment_type TEXT NOT NULL DEFAULT 'runtime',
                status           TEXT NOT NULL DEFAULT 'active',
                owner            TEXT NOT NULL DEFAULT 'unassigned',
                endpoint_url     TEXT,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL,
                FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_work_items_status ON work_items(status);
            CREATE INDEX IF NOT EXISTS idx_work_items_assignee ON work_items(assignee);
            CREATE INDEX IF NOT EXISTS idx_work_items_epic_id ON work_items(epic_id);
            CREATE INDEX IF NOT EXISTS idx_work_items_sprint_id ON work_items(sprint_id);
            CREATE INDEX IF NOT EXISTS idx_review_items_work_item_id ON review_items(work_item_id);
            CREATE INDEX IF NOT EXISTS idx_review_items_status ON review_items(status);
            CREATE INDEX IF NOT EXISTS idx_work_item_deps_work_item_id ON work_item_dependencies(work_item_id);
            CREATE INDEX IF NOT EXISTS idx_work_item_deps_depends_on_id ON work_item_dependencies(depends_on_id);
            CREATE INDEX IF NOT EXISTS idx_compliance_obligations_entity_id ON compliance_obligations(entity_id);
            CREATE INDEX IF NOT EXISTS idx_compliance_obligations_status ON compliance_obligations(status);
            CREATE INDEX IF NOT EXISTS idx_filings_entity_id ON filings(entity_id);
            CREATE INDEX IF NOT EXISTS idx_filings_obligation_id ON filings(obligation_id);
            CREATE INDEX IF NOT EXISTS idx_filings_due_date ON filings(due_date);
            CREATE INDEX IF NOT EXISTS idx_filings_status ON filings(status);
            CREATE INDEX IF NOT EXISTS idx_companies_status ON companies(status);
            CREATE INDEX IF NOT EXISTS idx_companies_owner ON companies(owner);
            CREATE INDEX IF NOT EXISTS idx_companies_operating_stage ON companies(operating_stage);
            CREATE INDEX IF NOT EXISTS idx_companies_compliance ON companies(compliance_sensitivity);
            CREATE INDEX IF NOT EXISTS idx_projects_company_id ON projects(company_id);
            CREATE INDEX IF NOT EXISTS idx_projects_status ON projects(status);
            CREATE INDEX IF NOT EXISTS idx_projects_owner ON projects(owner);
            CREATE INDEX IF NOT EXISTS idx_projects_operating_stage ON projects(operating_stage);
            CREATE INDEX IF NOT EXISTS idx_projects_compliance ON projects(compliance_sensitivity);
            CREATE INDEX IF NOT EXISTS idx_project_repos_project_id ON project_repos(project_id);
            CREATE INDEX IF NOT EXISTS idx_project_repos_status ON project_repos(status);
            CREATE INDEX IF NOT EXISTS idx_project_envs_project_id ON project_environments(project_id);
            CREATE INDEX IF NOT EXISTS idx_project_envs_status ON project_environments(status);
            ",
        )?;

        // Forward migration for databases created before task-group columns existed.
        ensure_column(&conn, "work_items", "task_group_id", "TEXT")?;
        ensure_column(&conn, "work_items", "sequence_order", "INTEGER")?;

        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_work_items_task_group_id ON work_items(task_group_id);
            CREATE INDEX IF NOT EXISTS idx_work_items_sequence_order ON work_items(sequence_order);
            ",
        )?;

        // Forward migration for portfolio fields.
        ensure_column(&conn, "companies", "business_unit", "TEXT")?;
        ensure_column(
            &conn,
            "companies",
            "status",
            "TEXT NOT NULL DEFAULT 'active'",
        )?;
        ensure_column(&conn, "companies", "priority", "INTEGER NOT NULL DEFAULT 3")?;
        ensure_column(
            &conn,
            "companies",
            "owner",
            "TEXT NOT NULL DEFAULT 'unassigned'",
        )?;
        ensure_column(
            &conn,
            "companies",
            "operating_stage",
            "TEXT NOT NULL DEFAULT 'build'",
        )?;
        ensure_column(
            &conn,
            "companies",
            "compliance_sensitivity",
            "TEXT NOT NULL DEFAULT 'moderate'",
        )?;
        ensure_column(
            &conn,
            "companies",
            "revenue_model_tags",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;

        ensure_column(&conn, "projects", "description", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(
            &conn,
            "projects",
            "status",
            "TEXT NOT NULL DEFAULT 'active'",
        )?;
        ensure_column(&conn, "projects", "priority", "INTEGER NOT NULL DEFAULT 3")?;
        ensure_column(
            &conn,
            "projects",
            "owner",
            "TEXT NOT NULL DEFAULT 'unassigned'",
        )?;
        ensure_column(
            &conn,
            "projects",
            "operating_stage",
            "TEXT NOT NULL DEFAULT 'build'",
        )?;
        ensure_column(
            &conn,
            "projects",
            "compliance_sensitivity",
            "TEXT NOT NULL DEFAULT 'moderate'",
        )?;
        ensure_column(
            &conn,
            "projects",
            "revenue_model_tags",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;

        ensure_column(
            &conn,
            "project_repos",
            "provider",
            "TEXT NOT NULL DEFAULT 'github'",
        )?;
        ensure_column(
            &conn,
            "project_repos",
            "default_branch",
            "TEXT NOT NULL DEFAULT 'main'",
        )?;
        ensure_column(
            &conn,
            "project_repos",
            "status",
            "TEXT NOT NULL DEFAULT 'active'",
        )?;

        ensure_column(
            &conn,
            "project_environments",
            "environment_type",
            "TEXT NOT NULL DEFAULT 'runtime'",
        )?;
        ensure_column(
            &conn,
            "project_environments",
            "status",
            "TEXT NOT NULL DEFAULT 'active'",
        )?;
        ensure_column(
            &conn,
            "project_environments",
            "owner",
            "TEXT NOT NULL DEFAULT 'unassigned'",
        )?;
        ensure_column(&conn, "project_environments", "endpoint_url", "TEXT")?;

        info!("Mission Control database migrations applied");
        Ok(())
    }

    /// Acquire the connection lock. Panics on poisoned mutex.
    ///
    /// Use this for all database operations.
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("McDb mutex poisoned")
    }
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    ddl_type: &str,
) -> anyhow::Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    if !columns.iter().any(|c| c == column) {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {ddl_type};"
        ))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let db = McDb::in_memory().unwrap();
        let conn = db.conn();
        // Verify tables exist
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('work_items','epics','sprints','review_items','work_item_dependencies','task_groups','legal_entities','compliance_obligations','filings','companies','projects','project_repos','project_environments')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 13);
    }
}
