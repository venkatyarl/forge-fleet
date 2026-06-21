//! Fleet topology ingestor for Cortex.
//!
//! This is a non-code ingestor: it mirrors the current fleet tables into the
//! existing `brain_vault_nodes` / `brain_vault_edges` graph without introducing
//! new storage. Postgres fleet tables remain the source of truth.

use anyhow::Result;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

const PROJECT: &str = "fleet";
const PROVENANCE: &str = "cortex-fleet";

#[derive(Debug, FromRow)]
struct ComputerRow {
    name: String,
}

#[derive(Debug, FromRow)]
struct DeploymentRow {
    id: Uuid,
    worker_name: String,
    model_id: Option<String>,
    runtime: String,
    port: i32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FleetIngestCounts {
    pub computers: i64,
    pub models: i64,
    pub deployments: i64,
    pub runs_on_edges: i64,
    pub serves_model_edges: i64,
}

impl FleetIngestCounts {
    pub fn nodes(self) -> i64 {
        self.computers + self.models + self.deployments
    }

    pub fn edges(self) -> i64 {
        self.runs_on_edges + self.serves_model_edges
    }
}

/// Ingest computers, model ids, and model deployments into the Cortex graph.
///
/// Returns the number of node + edge upsert attempts. Use [`fleet_counts`] for
/// the current materialized graph counts after the idempotent pass completes.
pub async fn ingest_fleet(pool: &PgPool) -> Result<usize> {
    let computers = load_computers(pool).await?;
    let deployments = load_deployments(pool).await?;

    let mut touched = 0usize;

    for computer in &computers {
        upsert_node(
            pool,
            &computer_path(&computer.name),
            &computer.name,
            "fleet:computer",
        )
        .await?;
        touched += 1;
    }

    for deployment in &deployments {
        if let Some(model_id) = deployment.model_id.as_deref() {
            upsert_node(pool, &model_path(model_id), model_id, "fleet:model").await?;
            touched += 1;
        }

        upsert_node(
            pool,
            &deployment_path(deployment.id),
            &deployment_title(deployment),
            "fleet:deployment",
        )
        .await?;
        touched += 1;
    }

    for deployment in &deployments {
        if add_edge(
            pool,
            &deployment_path(deployment.id),
            &computer_path(&deployment.worker_name),
            "runs_on",
        )
        .await?
        {
            touched += 1;
        }

        if let Some(model_id) = deployment.model_id.as_deref() {
            if add_edge(
                pool,
                &deployment_path(deployment.id),
                &model_path(model_id),
                "serves_model",
            )
            .await?
            {
                touched += 1;
            }
        }
    }

    Ok(touched)
}

pub async fn fleet_counts(pool: &PgPool) -> Result<FleetIngestCounts> {
    let computers = count_nodes(pool, "fleet:computer").await?;
    let models = count_nodes(pool, "fleet:model").await?;
    let deployments = count_nodes(pool, "fleet:deployment").await?;
    let runs_on_edges = count_edges(pool, "runs_on").await?;
    let serves_model_edges = count_edges(pool, "serves_model").await?;

    Ok(FleetIngestCounts {
        computers,
        models,
        deployments,
        runs_on_edges,
        serves_model_edges,
    })
}

async fn load_computers(pool: &PgPool) -> Result<Vec<ComputerRow>> {
    let rows = sqlx::query_as::<_, ComputerRow>(
        r#"
        SELECT name
          FROM (
                SELECT name FROM computers
                UNION
                SELECT name FROM fleet_workers
               ) fleet_computers
         WHERE name IS NOT NULL
           AND btrim(name) <> ''
         ORDER BY name
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn load_deployments(pool: &PgPool) -> Result<Vec<DeploymentRow>> {
    let rows = sqlx::query_as::<_, DeploymentRow>(
        r#"
        SELECT id,
               worker_name,
               NULLIF(btrim(catalog_id), '') AS model_id,
               runtime,
               port
          FROM fleet_model_deployments
         ORDER BY worker_name, port, id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn upsert_node(pool: &PgPool, path: &str, title: &str, node_type: &str) -> Result<Uuid> {
    let id = sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_nodes
            (path, title, node_type, project, content_hash, confidence, provenance)
        VALUES ($1, $2, $3, $4, $1, 1.0, $5)
        ON CONFLICT (path) DO UPDATE
          SET title = EXCLUDED.title,
              node_type = EXCLUDED.node_type,
              project = EXCLUDED.project,
              content_hash = EXCLUDED.content_hash,
              valid_until = NULL,
              updated_at = NOW(),
              confidence = GREATEST(brain_vault_nodes.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance
        RETURNING id
        "#,
    )
    .bind(path)
    .bind(title)
    .bind(node_type)
    .bind(PROJECT)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn add_edge(pool: &PgPool, src_path: &str, dst_path: &str, edge_type: &str) -> Result<bool> {
    let inserted = sqlx::query_scalar(
        r#"
        WITH endpoints AS (
            SELECT src.id AS src_id, dst.id AS dst_id
              FROM brain_vault_nodes src
              JOIN brain_vault_nodes dst ON dst.path = $2
             WHERE src.path = $1
        )
        INSERT INTO brain_vault_edges
            (src_id, dst_id, edge_type, provenance, confidence, method)
        SELECT src_id, dst_id, $3, $4, 1.0, 'TABLE'
          FROM endpoints
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance,
              method = EXCLUDED.method
        RETURNING (xmax = 0) AS inserted
        "#,
    )
    .bind(src_path)
    .bind(dst_path)
    .bind(edge_type)
    .bind(PROVENANCE)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.unwrap_or(false))
}

async fn count_nodes(pool: &PgPool, node_type: &str) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_nodes
         WHERE project = $1
           AND node_type = $2
           AND valid_until IS NULL
        "#,
    )
    .bind(PROJECT)
    .bind(node_type)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

async fn count_edges(pool: &PgPool, edge_type: &str) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = $1
           AND src.project = $2
           AND dst.project = $2
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .bind(edge_type)
    .bind(PROJECT)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

fn computer_path(name: &str) -> String {
    format!("fleet://computer/{name}")
}

fn model_path(model_id: &str) -> String {
    format!("fleet://model/{model_id}")
}

fn deployment_path(id: Uuid) -> String {
    format!("fleet://deployment/{id}")
}

fn deployment_title(deployment: &DeploymentRow) -> String {
    format!(
        "{}:{}:{}",
        deployment.worker_name, deployment.runtime, deployment.port
    )
}
