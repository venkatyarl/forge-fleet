//! Typed persistence model for project-management work items.
//!
//! This crate speaks plain `sqlx`, not an ActiveRecord-style ORM (Diesel /
//! SeaORM), so the three capabilities normally expressed as `Queryable`,
//! `Identifiable`, and `ActiveModel` traits are implemented as inherent
//! methods instead:
//! - **Queryable** — `#[derive(FromRow)]` lets any `sqlx::query_as::<_,
//!   WorkItem>(..)` decode a `work_items` row into this struct.
//! - **Identifiable** — [`WorkItem::find_by_id`] looks a row up by its
//!   primary key.
//! - **ActiveModel** — [`WorkItem::insert`] and [`WorkItem::update`] persist
//!   an in-memory `WorkItem` (e.g. one derived by decomposition/dispatch
//!   logic) as a new row or write its current field values back to its row.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::error::Result;

/// The persistent representation of a row in `work_items`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct WorkItem {
    pub id: Uuid,
    pub project_id: String,
    pub milestone_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub kind: String,
    pub title: String,
    pub description: Option<String>,
    pub labels: Value,
    pub status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub assigned_computer: Option<String>,
    pub branch_name: Option<String>,
    pub pr_url: Option<String>,
    pub brain_node_ids: Value,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    pub estimated_hours: Option<f64>,
    pub metadata: Value,
    pub required_capabilities: Value,
    pub complexity: String,
    pub predicted_paths: Value,
    pub touched_paths: Value,
    pub base_branch: Option<String>,
    pub base_sha: Option<String>,
    pub integration_branch: Option<String>,
    pub merge_rank: Option<i32>,
    pub risk_score: f32,
    pub reviewer_required: bool,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub repo_id: Option<Uuid>,
    pub repo_url: Option<String>,
    pub repo_path: Option<String>,
    pub context: Value,
    pub parked: bool,
    pub pre_work: Value,
    pub work: Value,
    pub post_work: Value,
    pub cleanup_complete: bool,
    pub original_signal: Value,
    pub signal_cleared: Option<bool>,
    pub signal_verified_at: Option<DateTime<Utc>>,
    pub refiled_from: Option<Uuid>,
    /// Cortex code-graph subgraph attached to this item, if any. V238.
    pub cortex_subgraph_id: Option<String>,
}

impl WorkItem {
    /// Identifiable: look up a single `work_items` row by its primary key.
    pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<WorkItem>> {
        let row = sqlx::query_as::<_, WorkItem>("SELECT * FROM work_items WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        Ok(row)
    }

    /// ActiveModel: persist this in-memory value as a brand-new row.
    pub async fn insert(&self, pool: &PgPool) -> Result<WorkItem> {
        let row = sqlx::query_as::<_, WorkItem>(
            r#"
            INSERT INTO work_items (
                id, project_id, milestone_id, parent_id, kind, title, description,
                labels, status, priority, assigned_to, assigned_computer, branch_name,
                pr_url, brain_node_ids, created_at, created_by, started_at, completed_at,
                due_date, estimated_hours, metadata, required_capabilities, complexity,
                predicted_paths, touched_paths, base_branch, base_sha, integration_branch,
                merge_rank, risk_score, reviewer_required, attempts, last_error, repo_id,
                repo_url, repo_path, context, parked, pre_work, work, post_work,
                cleanup_complete, original_signal, signal_cleared, signal_verified_at,
                refiled_from, cortex_subgraph_id
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30,
                $31, $32, $33, $34, $35, $36, $37, $38, $39, $40, $41, $42, $43, $44,
                $45, $46, $47, $48
            )
            RETURNING *
            "#,
        )
        .bind(self.id)
        .bind(&self.project_id)
        .bind(self.milestone_id)
        .bind(self.parent_id)
        .bind(&self.kind)
        .bind(&self.title)
        .bind(&self.description)
        .bind(&self.labels)
        .bind(&self.status)
        .bind(&self.priority)
        .bind(&self.assigned_to)
        .bind(&self.assigned_computer)
        .bind(&self.branch_name)
        .bind(&self.pr_url)
        .bind(&self.brain_node_ids)
        .bind(self.created_at)
        .bind(&self.created_by)
        .bind(self.started_at)
        .bind(self.completed_at)
        .bind(self.due_date)
        .bind(self.estimated_hours)
        .bind(&self.metadata)
        .bind(&self.required_capabilities)
        .bind(&self.complexity)
        .bind(&self.predicted_paths)
        .bind(&self.touched_paths)
        .bind(&self.base_branch)
        .bind(&self.base_sha)
        .bind(&self.integration_branch)
        .bind(self.merge_rank)
        .bind(self.risk_score)
        .bind(self.reviewer_required)
        .bind(self.attempts)
        .bind(&self.last_error)
        .bind(self.repo_id)
        .bind(&self.repo_url)
        .bind(&self.repo_path)
        .bind(&self.context)
        .bind(self.parked)
        .bind(&self.pre_work)
        .bind(&self.work)
        .bind(&self.post_work)
        .bind(self.cleanup_complete)
        .bind(&self.original_signal)
        .bind(self.signal_cleared)
        .bind(self.signal_verified_at)
        .bind(self.refiled_from)
        .bind(&self.cortex_subgraph_id)
        .fetch_one(pool)
        .await?;
        Ok(row)
    }

    /// ActiveModel: write this in-memory value's mutable fields back to its
    /// row (identified by `id`). `created_at`/`created_by` are treated as
    /// immutable provenance and are never overwritten.
    pub async fn update(&self, pool: &PgPool) -> Result<WorkItem> {
        let row = sqlx::query_as::<_, WorkItem>(
            r#"
            UPDATE work_items
            SET project_id = $1,
                milestone_id = $2,
                parent_id = $3,
                kind = $4,
                title = $5,
                description = $6,
                labels = $7,
                status = $8,
                priority = $9,
                assigned_to = $10,
                assigned_computer = $11,
                branch_name = $12,
                pr_url = $13,
                brain_node_ids = $14,
                started_at = $15,
                completed_at = $16,
                due_date = $17,
                estimated_hours = $18,
                metadata = $19,
                required_capabilities = $20,
                complexity = $21,
                predicted_paths = $22,
                touched_paths = $23,
                base_branch = $24,
                base_sha = $25,
                integration_branch = $26,
                merge_rank = $27,
                risk_score = $28,
                reviewer_required = $29,
                attempts = $30,
                last_error = $31,
                repo_id = $32,
                repo_url = $33,
                repo_path = $34,
                context = $35,
                parked = $36,
                pre_work = $37,
                work = $38,
                post_work = $39,
                cleanup_complete = $40,
                original_signal = $41,
                signal_cleared = $42,
                signal_verified_at = $43,
                refiled_from = $44,
                cortex_subgraph_id = $45
            WHERE id = $46
            RETURNING *
            "#,
        )
        .bind(&self.project_id)
        .bind(self.milestone_id)
        .bind(self.parent_id)
        .bind(&self.kind)
        .bind(&self.title)
        .bind(&self.description)
        .bind(&self.labels)
        .bind(&self.status)
        .bind(&self.priority)
        .bind(&self.assigned_to)
        .bind(&self.assigned_computer)
        .bind(&self.branch_name)
        .bind(&self.pr_url)
        .bind(&self.brain_node_ids)
        .bind(self.started_at)
        .bind(self.completed_at)
        .bind(self.due_date)
        .bind(self.estimated_hours)
        .bind(&self.metadata)
        .bind(&self.required_capabilities)
        .bind(&self.complexity)
        .bind(&self.predicted_paths)
        .bind(&self.touched_paths)
        .bind(&self.base_branch)
        .bind(&self.base_sha)
        .bind(&self.integration_branch)
        .bind(self.merge_rank)
        .bind(self.risk_score)
        .bind(self.reviewer_required)
        .bind(self.attempts)
        .bind(&self.last_error)
        .bind(self.repo_id)
        .bind(&self.repo_url)
        .bind(&self.repo_path)
        .bind(&self.context)
        .bind(self.parked)
        .bind(&self.pre_work)
        .bind(&self.work)
        .bind(&self.post_work)
        .bind(self.cleanup_complete)
        .bind(&self.original_signal)
        .bind(self.signal_cleared)
        .bind(self.signal_verified_at)
        .bind(self.refiled_from)
        .bind(&self.cortex_subgraph_id)
        .bind(self.id)
        .fetch_one(pool)
        .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    use sqlx::postgres::PgPoolOptions;

    fn base_db_url() -> Option<String> {
        env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()
    }

    /// Spins up a throwaway database with a full-column `work_items` table
    /// (mirroring the live schema) so CRUD tests never touch a shared DB.
    async fn temp_pool() -> Option<(PgPool, PgPool, String)> {
        let base_url = base_db_url()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_work_item_model_{}", uuid::Uuid::new_v4().simple());

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!("{prefix}/postgres"))
            .await
            .expect("connect to admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{prefix}/{db_name}"))
            .await
            .expect("connect to temp db");

        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE projects (id TEXT PRIMARY KEY);
             CREATE TABLE work_items (
                 id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 project_id             TEXT NOT NULL REFERENCES projects(id),
                 milestone_id           UUID,
                 parent_id              UUID REFERENCES work_items(id),
                 kind                   TEXT NOT NULL,
                 title                  TEXT NOT NULL,
                 description            TEXT,
                 labels                 JSONB NOT NULL DEFAULT '[]',
                 status                 TEXT NOT NULL DEFAULT 'idea',
                 priority               TEXT NOT NULL DEFAULT 'normal',
                 assigned_to            TEXT,
                 assigned_computer      TEXT,
                 branch_name            TEXT,
                 pr_url                 TEXT,
                 brain_node_ids         JSONB NOT NULL DEFAULT '[]',
                 created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                 created_by             TEXT NOT NULL DEFAULT 'system',
                 started_at             TIMESTAMPTZ,
                 completed_at           TIMESTAMPTZ,
                 due_date               DATE,
                 estimated_hours        DOUBLE PRECISION,
                 metadata               JSONB NOT NULL DEFAULT '{}',
                 required_capabilities  JSONB NOT NULL DEFAULT '[]',
                 complexity             TEXT NOT NULL DEFAULT 'mechanical',
                 predicted_paths        JSONB NOT NULL DEFAULT '[]',
                 touched_paths          JSONB NOT NULL DEFAULT '[]',
                 base_branch            TEXT,
                 base_sha               TEXT,
                 integration_branch     TEXT,
                 merge_rank             INT,
                 risk_score             REAL NOT NULL DEFAULT 0,
                 reviewer_required      BOOLEAN NOT NULL DEFAULT TRUE,
                 attempts               INT NOT NULL DEFAULT 0,
                 last_error             TEXT,
                 repo_id                UUID,
                 repo_url               TEXT,
                 repo_path              TEXT,
                 context                JSONB NOT NULL DEFAULT '{}',
                 parked                 BOOLEAN NOT NULL DEFAULT FALSE,
                 pre_work               JSONB NOT NULL DEFAULT '[]',
                 work                   JSONB NOT NULL DEFAULT '[]',
                 post_work              JSONB NOT NULL DEFAULT '[]',
                 cleanup_complete       BOOLEAN NOT NULL DEFAULT FALSE,
                 original_signal        JSONB NOT NULL DEFAULT '{}',
                 signal_cleared         BOOLEAN,
                 signal_verified_at     TIMESTAMPTZ,
                 refiled_from           UUID REFERENCES work_items(id),
                 cortex_subgraph_id     TEXT
             );",
        )
        .execute(&pool)
        .await
        .expect("create work_items schema");

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
        .expect("terminate temp db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }

    fn sample(project_id: &str) -> WorkItem {
        WorkItem {
            id: Uuid::new_v4(),
            project_id: project_id.to_string(),
            milestone_id: None,
            parent_id: None,
            kind: "task".to_string(),
            title: "derived work item".to_string(),
            description: None,
            labels: serde_json::json!([]),
            status: "idea".to_string(),
            priority: "normal".to_string(),
            assigned_to: None,
            assigned_computer: None,
            branch_name: None,
            pr_url: None,
            brain_node_ids: serde_json::json!([]),
            created_at: Utc::now(),
            created_by: "test".to_string(),
            started_at: None,
            completed_at: None,
            due_date: None,
            estimated_hours: None,
            metadata: serde_json::json!({}),
            required_capabilities: serde_json::json!([]),
            complexity: "mechanical".to_string(),
            predicted_paths: serde_json::json!([]),
            touched_paths: serde_json::json!([]),
            base_branch: None,
            base_sha: None,
            integration_branch: None,
            merge_rank: None,
            risk_score: 0.0,
            reviewer_required: true,
            attempts: 0,
            last_error: None,
            repo_id: None,
            repo_url: None,
            repo_path: None,
            context: serde_json::json!({}),
            parked: false,
            pre_work: serde_json::json!([]),
            work: serde_json::json!([]),
            post_work: serde_json::json!([]),
            cleanup_complete: false,
            original_signal: serde_json::json!({}),
            signal_cleared: None,
            signal_verified_at: None,
            refiled_from: None,
            cortex_subgraph_id: None,
        }
    }

    #[tokio::test]
    async fn insert_find_and_update_round_trip() {
        let Some((admin, pool, db_name)) = temp_pool().await else {
            return;
        };

        sqlx::query("INSERT INTO projects (id) VALUES ('p1')")
            .execute(&pool)
            .await
            .unwrap();

        let item = sample("p1");
        let inserted = item.insert(&pool).await.unwrap();
        assert_eq!(inserted.id, item.id);
        assert_eq!(inserted.title, "derived work item");
        assert_eq!(inserted.status, "idea");

        let found = WorkItem::find_by_id(&pool, item.id).await.unwrap();
        assert_eq!(found.map(|w| w.id), Some(item.id));

        let mut changed = inserted;
        changed.status = "ready".to_string();
        changed.attempts = 1;
        changed.cortex_subgraph_id = Some("subgraph-1".to_string());
        let updated = changed.update(&pool).await.unwrap();
        assert_eq!(updated.status, "ready");
        assert_eq!(updated.attempts, 1);
        assert_eq!(updated.cortex_subgraph_id.as_deref(), Some("subgraph-1"));

        let missing = WorkItem::find_by_id(&pool, Uuid::new_v4()).await.unwrap();
        assert!(missing.is_none());

        drop_temp_db(admin, pool, &db_name).await;
    }
}
