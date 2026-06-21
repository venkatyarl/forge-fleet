//! Local SQLite mirror for Cortex graph snapshots.
//!
//! Postgres remains the source of truth. This module writes and restores a
//! derived, portable snapshot for outage/wipe recovery and offline transport.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use sqlx::{FromRow, PgPool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, FromRow)]
struct PgNode {
    id: Uuid,
    path: String,
    title: String,
    node_type: Option<String>,
    project: Option<String>,
    confidence: Option<f32>,
    generation: Option<i64>,
    start_line: Option<i32>,
    end_line: Option<i32>,
}

#[derive(Debug, FromRow)]
struct PgEdge {
    src_id: Uuid,
    dst_id: Uuid,
    edge_type: String,
    confidence: f32,
    provenance: String,
    method: Option<String>,
    generation: Option<i64>,
}

#[derive(Debug)]
struct SqliteNode {
    id: String,
    path: String,
    title: String,
    node_type: Option<String>,
    project: Option<String>,
    confidence: Option<f32>,
    generation: Option<i64>,
    start_line: Option<i32>,
    end_line: Option<i32>,
}

#[derive(Debug)]
struct SqliteEdge {
    src_id: String,
    dst_id: String,
    edge_type: String,
    confidence: f32,
    provenance: String,
    method: Option<String>,
    generation: Option<i64>,
}

pub fn cache_path(corpus: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".forgefleet")
        .join("cortex-cache")
        .join(format!("{corpus}.db"))
}

pub fn counts(file: &Path) -> Result<(usize, usize)> {
    let conn =
        Connection::open(file).with_context(|| format!("open sqlite mirror {}", file.display()))?;
    let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
    let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
    Ok((nodes as usize, edges as usize))
}

pub async fn export(pool: &PgPool, corpus: &str, out: Option<PathBuf>) -> Result<PathBuf> {
    let path = out.unwrap_or_else(|| cache_path(corpus));
    let nodes = sqlx::query_as::<_, PgNode>(
        r#"
        SELECT id, path, title, node_type, project, confidence, generation, start_line, end_line
          FROM brain_vault_nodes
         WHERE project = $1
           AND valid_until IS NULL
           AND COALESCE(generation, 0) IN (
                0,
                COALESCE(
                    (SELECT current_generation FROM cortex_generations WHERE project = $1),
                    0
                )
           )
         ORDER BY path
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    let edges = sqlx::query_as::<_, PgEdge>(
        r#"
        SELECT e.src_id, e.dst_id, e.edge_type, e.confidence, e.provenance, e.method, e.generation
          FROM brain_vault_edges e
          JOIN brain_vault_nodes s ON s.id = e.src_id
          JOIN brain_vault_nodes d ON d.id = e.dst_id
         WHERE (s.project = $1 OR d.project = $1)
           AND COALESCE(e.generation, 0) IN (
                0,
                COALESCE(
                    (SELECT current_generation FROM cortex_generations WHERE project = $1),
                    0
                )
           )
         ORDER BY e.edge_type, e.src_id, e.dst_id
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create sqlite mirror directory {}", parent.display()))?;
    }
    let tmp_path = temp_path_for(&path)?;
    if tmp_path.exists() {
        std::fs::remove_file(&tmp_path)
            .with_context(|| format!("replace stale sqlite mirror temp {}", tmp_path.display()))?;
    }

    let mut conn = Connection::open(&tmp_path)
        .with_context(|| format!("create sqlite mirror {}", tmp_path.display()))?;
    init_schema(&conn)?;
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            r#"
            INSERT INTO nodes
                (id, path, title, node_type, project, confidence, generation, start_line, end_line)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
        )?;
        for node in &nodes {
            stmt.execute(params![
                node.id.to_string(),
                node.path,
                node.title,
                node.node_type,
                node.project,
                node.confidence,
                node.generation,
                node.start_line,
                node.end_line,
            ])?;
        }
    }
    {
        let mut stmt = tx.prepare(
            r#"
            INSERT INTO edges
                (src_id, dst_id, edge_type, confidence, provenance, method, generation)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
        )?;
        for edge in &edges {
            stmt.execute(params![
                edge.src_id.to_string(),
                edge.dst_id.to_string(),
                edge.edge_type,
                edge.confidence,
                edge.provenance,
                edge.method,
                edge.generation,
            ])?;
        }
    }
    tx.commit()?;
    drop(conn);

    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("install sqlite mirror {}", path.display()))?;

    Ok(path)
}

pub async fn import(
    pool: &PgPool,
    file: &Path,
    corpus_override: Option<&str>,
) -> Result<(usize, usize)> {
    let conn =
        Connection::open(file).with_context(|| format!("open sqlite mirror {}", file.display()))?;
    let nodes = read_nodes(&conn)?;
    let edges = read_edges(&conn)?;

    let mut tx = pool.begin().await?;
    let mut id_map: HashMap<String, Uuid> = HashMap::with_capacity(nodes.len());
    let mut restored_nodes = 0usize;

    for node in nodes {
        let snapshot_id = Uuid::parse_str(&node.id)
            .with_context(|| format!("parse mirror node id {}", node.id))?;
        let project = corpus_override
            .map(str::to_string)
            .or_else(|| node.project.clone());
        let restored_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO brain_vault_nodes
                (id, path, title, node_type, project, content_hash, confidence,
                 generation, start_line, end_line, provenance)
            VALUES ($1, $2, $3, $4, $5, $2, $6, $7, $8, $9, 'cortex-mirror')
            ON CONFLICT (path) DO UPDATE
              SET title = EXCLUDED.title,
                  node_type = EXCLUDED.node_type,
                  project = EXCLUDED.project,
                  content_hash = EXCLUDED.content_hash,
                  valid_until = NULL,
                  updated_at = NOW(),
                  confidence = COALESCE(EXCLUDED.confidence, brain_vault_nodes.confidence),
                  generation = EXCLUDED.generation,
                  start_line = EXCLUDED.start_line,
                  end_line = EXCLUDED.end_line,
                  provenance = COALESCE(brain_vault_nodes.provenance, EXCLUDED.provenance)
            RETURNING id
            "#,
        )
        .bind(snapshot_id)
        .bind(&node.path)
        .bind(&node.title)
        .bind(&node.node_type)
        .bind(&project)
        .bind(node.confidence)
        .bind(node.generation)
        .bind(node.start_line)
        .bind(node.end_line)
        .fetch_one(&mut *tx)
        .await?;
        id_map.insert(node.id, restored_id);
        restored_nodes += 1;
    }

    let mut restored_edges = 0usize;
    for edge in edges {
        let Some(src_id) = id_map.get(&edge.src_id).copied() else {
            continue;
        };
        let Some(dst_id) = id_map.get(&edge.dst_id).copied() else {
            continue;
        };
        sqlx::query(
            r#"
            INSERT INTO brain_vault_edges
                (src_id, dst_id, edge_type, confidence, provenance, method, generation)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
              SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
                  provenance = EXCLUDED.provenance,
                  method = COALESCE(EXCLUDED.method, brain_vault_edges.method),
                  generation = EXCLUDED.generation
            "#,
        )
        .bind(src_id)
        .bind(dst_id)
        .bind(&edge.edge_type)
        .bind(edge.confidence)
        .bind(&edge.provenance)
        .bind(&edge.method)
        .bind(edge.generation)
        .execute(&mut *tx)
        .await?;
        restored_edges += 1;
    }

    tx.commit().await?;
    Ok((restored_nodes, restored_edges))
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = DELETE;

        CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            path TEXT,
            title TEXT,
            node_type TEXT,
            project TEXT,
            confidence REAL,
            generation INTEGER,
            start_line INTEGER,
            end_line INTEGER
        );

        CREATE TABLE edges (
            src_id TEXT,
            dst_id TEXT,
            edge_type TEXT,
            confidence REAL,
            provenance TEXT,
            method TEXT,
            generation INTEGER
        );

        CREATE INDEX idx_nodes_project ON nodes(project);
        CREATE INDEX idx_edges_src ON edges(src_id, edge_type);
        CREATE INDEX idx_edges_dst ON edges(dst_id, edge_type);
        "#,
    )?;
    Ok(())
}

fn temp_path_for(path: &Path) -> Result<PathBuf> {
    let file_name = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
        anyhow::anyhow!("sqlite mirror path has no file name: {}", path.display())
    })?;
    Ok(path.with_file_name(format!("{file_name}.tmp")))
}

fn read_nodes(conn: &Connection) -> Result<Vec<SqliteNode>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, path, title, node_type, project, confidence, generation, start_line, end_line
          FROM nodes
         ORDER BY path
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SqliteNode {
            id: row.get(0)?,
            path: row.get(1)?,
            title: row.get(2)?,
            node_type: row.get(3)?,
            project: row.get(4)?,
            confidence: row.get(5)?,
            generation: row.get(6)?,
            start_line: row.get(7)?,
            end_line: row.get(8)?,
        })
    })?;
    collect_rows(rows)
}

fn read_edges(conn: &Connection) -> Result<Vec<SqliteEdge>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT src_id, dst_id, edge_type, confidence, provenance, method, generation
          FROM edges
         ORDER BY edge_type, src_id, dst_id
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SqliteEdge {
            src_id: row.get(0)?,
            dst_id: row.get(1)?,
            edge_type: row.get(2)?,
            confidence: row.get(3)?,
            provenance: row.get(4)?,
            method: row.get(5)?,
            generation: row.get(6)?,
        })
    })?;
    collect_rows(rows)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    if out.is_empty() {
        return Ok(out);
    }
    Ok(out)
}
