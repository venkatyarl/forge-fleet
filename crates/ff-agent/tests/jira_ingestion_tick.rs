//! Integration test for the Jira queue ingestion tick.
//!
//! Skips when neither ForgeFleet Postgres URL is configured because database
//! access is intentionally unavailable in the library-only CI test job.

use std::env;
use std::path::PathBuf;

use axum::{Json, Router, routing::get};
use ff_agent::tools::{
    AgentTool, AgentToolContext, checkout_edit_lock, jira::JiraQueueTool, session_shell_state,
};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::net::TcpListener;
use uuid::Uuid;

fn temp_db_urls() -> Option<(String, String, String)> {
    let base_url = env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
        .ok()?;
    let (prefix, _) = base_url.rsplit_once('/')?;
    let db_name = format!("ff_jira_ingestion_it_{}", Uuid::new_v4().simple());
    Some((
        format!("{prefix}/postgres"),
        format!("{prefix}/{db_name}"),
        db_name,
    ))
}

async fn create_test_db() -> Option<(PgPool, PgPool, String)> {
    let (admin_url, db_url, db_name) = temp_db_urls()?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect admin db");
    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin)
        .await
        .expect("create temp db");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("connect temp db");

    sqlx::raw_sql(
        "CREATE EXTENSION IF NOT EXISTS pgcrypto;
         CREATE TABLE projects (id TEXT PRIMARY KEY);
         CREATE TABLE jira_rulesets (id TEXT PRIMARY KEY);
         CREATE TABLE jira_configs (
             name TEXT PRIMARY KEY,
             project_key TEXT NOT NULL,
             owner_account_id TEXT NOT NULL,
             jira_secret_ref TEXT NOT NULL,
             queue_jql TEXT NOT NULL,
             ruleset_id TEXT NOT NULL REFERENCES jira_rulesets(id),
             repo_map_json JSONB NOT NULL DEFAULT '{}'
         );
         CREATE TABLE project_repos (
             id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
             project_id TEXT NOT NULL REFERENCES projects(id),
             github_url TEXT NOT NULL,
             name TEXT,
             default_branch TEXT NOT NULL DEFAULT 'main'
         );
         CREATE TABLE fleet_secrets (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE work_items (
             id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
             project_id TEXT NOT NULL REFERENCES projects(id),
             kind TEXT NOT NULL,
             title TEXT NOT NULL,
             description TEXT,
             labels JSONB NOT NULL DEFAULT '[]',
             status TEXT NOT NULL,
             priority TEXT NOT NULL,
             created_by TEXT NOT NULL,
             metadata JSONB NOT NULL DEFAULT '{}',
             repo_id UUID REFERENCES project_repos(id),
             repo_url TEXT,
             base_branch TEXT,
             original_signal JSONB NOT NULL DEFAULT '{}'
         );",
    )
    .execute(&pool)
    .await
    .expect("create Jira ingestion test schema");

    Some((admin, pool, db_name))
}

async fn drop_test_db(admin: PgPool, pool: PgPool, db_name: &str) {
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
async fn jira_ingestion_tick_creates_a_work_item_from_mocked_issue() {
    let Some((admin, pool, db_name)) = create_test_db().await else {
        eprintln!(
            "skipping Jira ingestion integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let app = Router::new().route(
        "/rest/api/3/search/jql",
        get(|| async {
            Json(json!({
                "issues": [{
                    "id": "10001",
                    "key": "HFPROD-268",
                    "fields": {
                        "summary": "Cycle allocation APIs",
                        "description": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "content": [{"type": "text", "text": "Implement allocation."}]
                            }]
                        },
                        "labels": ["hireflow360-api"],
                        "priority": {"name": "Highest"},
                        "status": {"name": "To Do"}
                    }
                }]
            }))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Jira server");
    let jira_base_url = format!(
        "http://{}",
        listener.local_addr().expect("mock server address")
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock Jira response");
    });

    sqlx::query("INSERT INTO projects (id) VALUES ('hireflow360')")
        .execute(&pool)
        .await
        .expect("insert project");
    sqlx::query("INSERT INTO jira_rulesets (id) VALUES ('hireflow360-v1')")
        .execute(&pool)
        .await
        .expect("insert Jira ruleset");
    sqlx::query(
        "INSERT INTO jira_configs
             (name, project_key, owner_account_id, jira_secret_ref, queue_jql, ruleset_id,
              repo_map_json)
         VALUES
             ('hireflow360', 'HFPROD', 'account-1', 'jira.test.token',
              'project = HFPROD', 'hireflow360-v1',
              '{\"hireflow360-api\":\"hireflow360-api\"}')",
    )
    .execute(&pool)
    .await
    .expect("insert Jira config");
    sqlx::query(
        "INSERT INTO project_repos (project_id, github_url, name, default_branch)
         VALUES ('hireflow360', 'git@example.test:hireflow360-api.git',
                 'hireflow360-api', 'develop')",
    )
    .execute(&pool)
    .await
    .expect("insert project repo");
    for (key, value) in [
        ("jira.hireflow360.base_url", jira_base_url.as_str()),
        ("jira.hireflow360.auth_email", "agent@example.test"),
        ("jira.test.token", "test-token"),
    ] {
        sqlx::query("INSERT INTO fleet_secrets (key, value) VALUES ($1, $2)")
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .expect("insert Jira secret");
    }

    let working_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let context = AgentToolContext {
        working_dir: working_dir.clone(),
        session_id: "jira-ingestion-test".to_owned(),
        shell_state: session_shell_state("jira-ingestion-test"),
        edit_lock: checkout_edit_lock(&working_dir),
        pg_pool: Some(pool.clone()),
    };
    let result = JiraQueueTool::default()
        .execute(json!({"config": "hireflow360"}), &context)
        .await;
    assert!(!result.is_error, "Jira tick failed: {}", result.content);
    let output: Value = serde_json::from_str(&result.content).expect("parse Jira tick result");
    assert_eq!(output["fetched"], 1);
    assert_eq!(output["created"], 1);

    let row = sqlx::query(
        "SELECT project_id, kind, title, description, labels, status, priority, created_by,
                metadata, repo_url, base_branch, original_signal
           FROM work_items",
    )
    .fetch_one(&pool)
    .await
    .expect("read ingested work item");
    assert_eq!(row.get::<String, _>("project_id"), "hireflow360");
    assert_eq!(row.get::<String, _>("kind"), "jira");
    assert_eq!(
        row.get::<String, _>("title"),
        "HFPROD-268 Cycle allocation APIs"
    );
    assert_eq!(
        row.get::<Option<String>, _>("description").as_deref(),
        Some("Implement allocation.")
    );
    assert_eq!(row.get::<Value, _>("labels"), json!(["hireflow360-api"]));
    assert_eq!(row.get::<String, _>("status"), "ready");
    assert_eq!(row.get::<String, _>("priority"), "critical");
    assert_eq!(row.get::<String, _>("created_by"), "jira_queue_poll");
    assert_eq!(
        row.get::<String, _>("repo_url"),
        "git@example.test:hireflow360-api.git"
    );
    assert_eq!(row.get::<String, _>("base_branch"), "develop");
    assert_eq!(
        row.get::<Value, _>("metadata")["jira_issue_key"],
        "HFPROD-268"
    );
    assert_eq!(
        row.get::<Value, _>("original_signal")["signature"],
        "jira:hireflow360:10001"
    );

    server.abort();
    drop_test_db(admin, pool, &db_name).await;
}
