//! Canonical people ingestor for Cortex.
//!
//! The owners extractor writes per-corpus `person:dev` nodes from git history.
//! This ingestor unifies those aliases into fleet-wide canonical person nodes
//! and duplicates authorship edges onto the canonical identity.

use anyhow::Result;
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use std::collections::BTreeMap;
use uuid::Uuid;

const PROJECT: &str = "people";
const PROVENANCE: &str = "cortex-people";

#[derive(Debug, FromRow)]
struct PersonAliasRow {
    id: Uuid,
    title: String,
}

#[derive(Debug, FromRow)]
struct OwnedFileRow {
    person_id: Uuid,
    file_id: Uuid,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PeopleIngestCounts {
    pub canonical_people: i64,
    pub aliases: i64,
    pub authored_edges: i64,
    pub alias_of_edges: i64,
}

impl PeopleIngestCounts {
    pub fn edges(self) -> i64 {
        self.authored_edges + self.alias_of_edges
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PersonSummary {
    pub name: String,
    pub path: String,
    pub file_count: i64,
    pub repo_count: i64,
}

/// Create canonical `person:dev` nodes and connect per-corpus aliases + files.
///
/// Reads all existing per-corpus `person:dev` nodes and their `owns` edges
/// without a `current_generation` filter so same-pass Cortex facts are visible.
pub async fn ingest_people(pool: &PgPool) -> Result<usize> {
    let aliases = load_person_aliases(pool).await?;
    let owned_files = load_owned_files(pool).await?;

    let mut canonical_by_alias = BTreeMap::new();
    let mut display_by_key = BTreeMap::new();
    for alias in &aliases {
        let Some(key) = normalize_person_name(&alias.title) else {
            continue;
        };
        canonical_by_alias.insert(alias.id, key.clone());
        display_by_key
            .entry(key)
            .and_modify(|existing: &mut String| {
                if alias.title.len() < existing.len() {
                    *existing = alias.title.clone();
                }
            })
            .or_insert_with(|| alias.title.clone());
    }

    let mut touched = 0usize;
    let mut canonical_ids = BTreeMap::new();
    for (key, display_name) in &display_by_key {
        let id = upsert_node(pool, &canonical_path(key), display_name, "person:dev").await?;
        canonical_ids.insert(key.clone(), id);
        touched += 1;
    }

    for alias in &aliases {
        let Some(key) = canonical_by_alias.get(&alias.id) else {
            continue;
        };
        let Some(canonical_id) = canonical_ids.get(key) else {
            continue;
        };
        if add_edge(pool, alias.id, *canonical_id, "alias_of").await? {
            touched += 1;
        }
    }

    for owned in &owned_files {
        let Some(key) = canonical_by_alias.get(&owned.person_id) else {
            continue;
        };
        let Some(canonical_id) = canonical_ids.get(key) else {
            continue;
        };
        if add_edge(pool, *canonical_id, owned.file_id, "authored").await? {
            touched += 1;
        }
    }

    Ok(touched)
}

pub async fn people_counts(pool: &PgPool) -> Result<PeopleIngestCounts> {
    let canonical_people = count_canonical_people(pool).await?;
    let aliases = count_aliases(pool).await?;
    let authored_edges = count_edges_from_canonical(pool, "authored").await?;
    let alias_of_edges = count_alias_of_edges(pool).await?;

    Ok(PeopleIngestCounts {
        canonical_people,
        aliases,
        authored_edges,
        alias_of_edges,
    })
}

pub async fn people(pool: &PgPool) -> Result<Vec<PersonSummary>> {
    let rows = sqlx::query_as::<_, PersonSummary>(
        r#"
        SELECT p.title AS name,
               p.path AS path,
               COUNT(DISTINCT f.id)::bigint AS file_count,
               COUNT(DISTINCT f.project)::bigint AS repo_count
          FROM brain_vault_nodes p
          LEFT JOIN brain_vault_edges e
            ON e.src_id = p.id
           AND e.edge_type = 'authored'
          LEFT JOIN brain_vault_nodes f
            ON f.id = e.dst_id
           AND f.node_type = 'content:file'
           AND f.valid_until IS NULL
         WHERE p.node_type = 'person:dev'
           AND p.path LIKE 'person://canonical/%'
           AND p.valid_until IS NULL
         GROUP BY p.title, p.path
         ORDER BY file_count DESC, repo_count DESC, p.title
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn load_person_aliases(pool: &PgPool) -> Result<Vec<PersonAliasRow>> {
    let rows = sqlx::query_as::<_, PersonAliasRow>(
        r#"
        SELECT id, title
          FROM brain_vault_nodes
         WHERE node_type = 'person:dev'
           AND path NOT LIKE 'person://canonical/%'
           AND valid_until IS NULL
         ORDER BY project, title, path
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn load_owned_files(pool: &PgPool) -> Result<Vec<OwnedFileRow>> {
    let rows = sqlx::query_as::<_, OwnedFileRow>(
        r#"
        SELECT p.id AS person_id,
               f.id AS file_id
          FROM brain_vault_nodes p
          JOIN brain_vault_edges e
            ON e.src_id = p.id
           AND e.edge_type = 'owns'
          JOIN brain_vault_nodes f
            ON f.id = e.dst_id
           AND f.node_type = 'content:file'
           AND f.valid_until IS NULL
         WHERE p.node_type = 'person:dev'
           AND p.path NOT LIKE 'person://canonical/%'
           AND p.valid_until IS NULL
         ORDER BY p.id, f.id
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

async fn add_edge(pool: &PgPool, src: Uuid, dst: Uuid, edge_type: &str) -> Result<bool> {
    let inserted = sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_edges
            (src_id, dst_id, edge_type, provenance, confidence, method)
        VALUES ($1, $2, $3, $4, 1.0, 'TABLE')
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance,
              method = EXCLUDED.method
        RETURNING (xmax = 0) AS inserted
        "#,
    )
    .bind(src)
    .bind(dst)
    .bind(edge_type)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;
    Ok(inserted)
}

async fn count_canonical_people(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_nodes
         WHERE node_type = 'person:dev'
           AND path LIKE 'person://canonical/%'
           AND valid_until IS NULL
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

async fn count_aliases(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT e.src_id)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = 'alias_of'
           AND src.node_type = 'person:dev'
           AND src.path NOT LIKE 'person://canonical/%'
           AND dst.path LIKE 'person://canonical/%'
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

async fn count_edges_from_canonical(pool: &PgPool, edge_type: &str) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = $1
           AND src.node_type = 'person:dev'
           AND src.path LIKE 'person://canonical/%'
           AND dst.valid_until IS NULL
           AND src.valid_until IS NULL
        "#,
    )
    .bind(edge_type)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

async fn count_alias_of_edges(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = 'alias_of'
           AND src.node_type = 'person:dev'
           AND src.path NOT LIKE 'person://canonical/%'
           AND dst.path LIKE 'person://canonical/%'
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

fn normalize_person_name(name: &str) -> Option<String> {
    let normalized = name.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn canonical_path(normalized: &str) -> String {
    format!("person://canonical/{normalized}")
}
