//! Node drain handler.
//!
//! Processes a request to take one computer out of the sub-agent work
//! rotation (maintenance, decommission, or any caller besides `fleet deploy
//! --graceful`, which already inlines this sequence in
//! `ff-terminal`'s `drain_deploy_targets`): every sub-agent slot on that
//! computer is disabled so the claim queries in `ff-db` (see the "operator
//! quarantine" comment on `pg_complete_parent_work_items`) and
//! `work_item_feeder` stop assigning it new work, and any work item it
//! currently has claimed/building is released back to `ready` via the
//! existing attempt-neutral lease drain ([`super::drain_work_item_leases`]).
//!
//! The request-processing path is `ff fleet drain <computer>`
//! (`ff-terminal`'s `handle_fleet_drain`, wired through `FleetCommand::Drain`
//! in `main.rs`), which resolves the operator-supplied computer name to its
//! `computers.id` and calls [`drain_node`] below.

use sqlx::PgPool;
use uuid::Uuid;

/// Result of processing a single node drain request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrainResult {
    /// Sub-agent slots on this computer flipped to `disabled`.
    pub sub_agents_disabled: u64,
    /// In-flight work items released back to `ready`.
    pub work_items_released: u64,
}

/// Process a drain request for `computer_id`: disable its sub-agent slots
/// and release any in-flight work back to `ready` so other nodes can pick it
/// up. Idempotent — draining an already-drained node disables zero
/// additional slots and releases zero additional work items.
pub async fn drain_node(pool: &PgPool, computer_id: Uuid) -> Result<DrainResult, sqlx::Error> {
    let sub_agents_disabled: i64 = sqlx::query_scalar(
        "WITH disabled AS (
             UPDATE sub_agents
                SET status = 'disabled'
              WHERE computer_id = $1
                AND status <> 'disabled'
          RETURNING id
         )
         SELECT COUNT(*) FROM disabled",
    )
    .bind(computer_id)
    .fetch_one(pool)
    .await?;

    let work_items_released = super::drain_work_item_leases(pool, computer_id).await?;

    Ok(DrainResult {
        sub_agents_disabled: sub_agents_disabled as u64,
        work_items_released,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::env;

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_node_drain_int_{}", Uuid::new_v4().simple());
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
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE computers (
                 id   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 name TEXT NOT NULL UNIQUE
             );
             CREATE TABLE projects (
                 id TEXT PRIMARY KEY
             );
             CREATE TABLE work_items (
                 id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 project_id        TEXT NOT NULL REFERENCES projects(id),
                 kind              TEXT NOT NULL DEFAULT 'task',
                 title             TEXT NOT NULL DEFAULT '',
                 status            TEXT NOT NULL DEFAULT 'idea',
                 assigned_computer TEXT
             );
             CREATE TABLE sub_agents (
                 id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 computer_id          UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
                 slot                 INT NOT NULL,
                 status               TEXT NOT NULL DEFAULT 'idle',
                 current_work_item_id UUID REFERENCES work_items(id),
                 started_at           TIMESTAMPTZ,
                 workspace_dir        TEXT NOT NULL DEFAULT '',
                 last_heartbeat_at    TIMESTAMPTZ,
                 UNIQUE (computer_id, slot)
             );
             CREATE TABLE work_item_leases (
                 id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id     UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
                 sub_agent_id     UUID REFERENCES sub_agents(id),
                 computer_id      UUID NOT NULL REFERENCES computers(id),
                 endpoint         TEXT,
                 lease_state      TEXT NOT NULL DEFAULT 'claimed',
                 lease_expires_at TIMESTAMPTZ NOT NULL DEFAULT NOW() + INTERVAL '1 hour',
                 attempt          INT NOT NULL DEFAULT 1,
                 released_at      TIMESTAMPTZ,
                 release_reason   TEXT
             );
             CREATE TABLE work_item_worktrees (
                 id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id  UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
                 computer_id   UUID NOT NULL REFERENCES computers(id),
                 sub_agent_id  UUID REFERENCES sub_agents(id),
                 repo_path     TEXT NOT NULL DEFAULT '',
                 worktree_path TEXT NOT NULL DEFAULT '',
                 base_branch   TEXT NOT NULL DEFAULT 'main',
                 task_branch   TEXT NOT NULL DEFAULT 'task',
                 status        TEXT NOT NULL DEFAULT 'active'
             );
             CREATE TABLE work_item_events (
                 id            BIGSERIAL PRIMARY KEY,
                 work_item_id  UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
                 from_status   TEXT,
                 to_status     TEXT NOT NULL,
                 computer      TEXT,
                 attempt       INTEGER,
                 detail        JSONB NOT NULL DEFAULT '{}'::jsonb
             );",
        )
        .execute(&pool)
        .await
        .expect("create minimal node-drain schema");
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

    #[tokio::test]
    async fn drain_node_disables_slots_and_requeues_in_flight_work() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping drain_node_disables_slots_and_requeues_in_flight_work: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        let computer_id: Uuid =
            sqlx::query_scalar("INSERT INTO computers (name) VALUES ('node-a') RETURNING id")
                .fetch_one(&pool)
                .await
                .expect("insert computer");

        let busy_slot_id: Uuid = sqlx::query_scalar(
            "INSERT INTO sub_agents (computer_id, slot, status, workspace_dir)
             VALUES ($1, 1, 'busy', '') RETURNING id",
        )
        .bind(computer_id)
        .fetch_one(&pool)
        .await
        .expect("insert busy sub-agent");

        sqlx::query(
            "INSERT INTO sub_agents (computer_id, slot, status, workspace_dir)
             VALUES ($1, 0, 'idle', '')",
        )
        .bind(computer_id)
        .execute(&pool)
        .await
        .expect("insert idle sub-agent");

        sqlx::query("INSERT INTO projects (id) VALUES ('p1')")
            .execute(&pool)
            .await
            .expect("insert project");

        let work_item_id: Uuid = sqlx::query_scalar(
            "INSERT INTO work_items (project_id, kind, title, status, assigned_computer)
             VALUES ('p1', 'task', 'do the thing', 'building', 'node-a') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert work item");

        sqlx::query("UPDATE sub_agents SET current_work_item_id = $1 WHERE id = $2")
            .bind(work_item_id)
            .bind(busy_slot_id)
            .execute(&pool)
            .await
            .expect("assign work item to busy slot");

        sqlx::query(
            "INSERT INTO work_item_leases (work_item_id, sub_agent_id, computer_id, lease_state, attempt)
             VALUES ($1, $2, $3, 'building', 1)",
        )
        .bind(work_item_id)
        .bind(busy_slot_id)
        .bind(computer_id)
        .execute(&pool)
        .await
        .expect("insert lease");

        let result = drain_node(&pool, computer_id).await.expect("drain node");
        assert_eq!(result.sub_agents_disabled, 2);
        assert_eq!(result.work_items_released, 1);

        let statuses: Vec<String> =
            sqlx::query("SELECT status FROM sub_agents WHERE computer_id = $1")
                .bind(computer_id)
                .fetch_all(&pool)
                .await
                .expect("fetch sub-agent statuses")
                .into_iter()
                .map(|row| row.get("status"))
                .collect();
        assert!(statuses.iter().all(|s| s == "disabled"));

        let current_work_item: Option<Uuid> =
            sqlx::query_scalar("SELECT current_work_item_id FROM sub_agents WHERE id = $1")
                .bind(busy_slot_id)
                .fetch_one(&pool)
                .await
                .expect("fetch busy slot");
        assert!(current_work_item.is_none());

        let (wi_status, wi_assigned): (String, Option<String>) =
            sqlx::query_as("SELECT status, assigned_computer FROM work_items WHERE id = $1")
                .bind(work_item_id)
                .fetch_one(&pool)
                .await
                .expect("fetch work item");
        assert_eq!(wi_status, "ready");
        assert!(wi_assigned.is_none());

        let released_at: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT released_at FROM work_item_leases WHERE work_item_id = $1")
                .bind(work_item_id)
                .fetch_one(&pool)
                .await
                .expect("fetch lease");
        assert!(released_at.is_some());

        // Draining an already-drained node is a no-op.
        let repeat = drain_node(&pool, computer_id).await.expect("re-drain node");
        assert_eq!(repeat.sub_agents_disabled, 0);
        assert_eq!(repeat.work_items_released, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }
}
