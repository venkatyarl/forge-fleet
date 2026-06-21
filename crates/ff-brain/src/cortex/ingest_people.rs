//! Cross-corpus person canonicalization ingestor for Cortex.
//!
//! The owners extractor creates per-corpus `person:dev` nodes from git history.
//! This pass adds canonical fleet-wide people and duplicates ownership from each
//! per-corpus person onto the canonical node.

use anyhow::Result;
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

const PROJECT: &str = "people";
const PROVENANCE: &str = "cortex-people";

#[derive(Debug, Clone)]
struct OwnershipRow {
    person_path: String,
    person_title: String,
    file_path: Option<String>,
}

#[derive(Debug, Clone)]
struct CanonicalPerson {
    normalized: String,
    title: String,
    aliases: BTreeSet<String>,
    files: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PeopleIngestCounts {
    pub canonical_people: i64,
    pub authored_edges: i64,
    pub alias_of_edges: i64,
    pub upsert_attempts: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonSummary {
    pub name: String,
    pub normalized: String,
    pub file_count: i64,
    pub corpus_count: i64,
    pub path: String,
}

pub async fn ingest_people(pool: &PgPool) -> Result<PeopleIngestCounts> {
    let rows = load_ownership(pool).await?;
    let people = canonicalize(rows);
    let mut upsert_attempts = 0usize;

    for person in people.values() {
        upsert_node(
            pool,
            &canonical_path(&person.normalized),
            &person.title,
            "person:dev",
        )
        .await?;
        upsert_attempts += 1;
    }

    for person in people.values() {
        let canonical = canonical_path(&person.normalized);
        for alias in &person.aliases {
            if add_edge(pool, alias, &canonical, "alias_of").await? {
                upsert_attempts += 1;
            }
        }
        for file in &person.files {
            if add_edge(pool, &canonical, file, "authored").await? {
                upsert_attempts += 1;
            }
        }
    }

    let mut counts = people_counts(pool).await?;
    counts.upsert_attempts = upsert_attempts;
    Ok(counts)
}

pub async fn people_counts(pool: &PgPool) -> Result<PeopleIngestCounts> {
    let canonical_people = count_canonical_people(pool).await?;
    let authored_edges = count_canonical_edges(pool, "authored").await?;
    let alias_of_edges = count_canonical_edges(pool, "alias_of").await?;
    Ok(PeopleIngestCounts {
        canonical_people,
        authored_edges,
        alias_of_edges,
        upsert_attempts: 0,
    })
}

pub async fn list_people(pool: &PgPool) -> Result<Vec<PersonSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT p.title AS name,
               replace(p.path, 'person://canonical/', '') AS normalized,
               p.path AS path,
               COUNT(DISTINCT f.id)::bigint AS file_count,
               COUNT(DISTINCT f.project)::bigint AS corpus_count
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
         ORDER BY file_count DESC, lower(p.title), p.path
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| PersonSummary {
            name: row.get("name"),
            normalized: row.get("normalized"),
            file_count: row.get("file_count"),
            corpus_count: row.get("corpus_count"),
            path: row.get("path"),
        })
        .collect())
}

async fn load_ownership(pool: &PgPool) -> Result<Vec<OwnershipRow>> {
    let rows = sqlx::query(
        r#"
        SELECT p.path AS person_path,
               p.title AS person_title,
               f.path AS file_path
          FROM brain_vault_nodes p
          LEFT JOIN brain_vault_edges e
            ON e.src_id = p.id
           AND e.edge_type = 'owns'
          LEFT JOIN brain_vault_nodes f
            ON f.id = e.dst_id
           AND f.node_type = 'content:file'
           AND f.valid_until IS NULL
         WHERE p.node_type = 'person:dev'
           AND p.valid_until IS NULL
           AND p.path NOT LIKE 'person://canonical/%'
         ORDER BY lower(p.title), p.path, f.path
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| OwnershipRow {
            person_path: row.get("person_path"),
            person_title: row.get("person_title"),
            file_path: row.get("file_path"),
        })
        .collect())
}

fn canonicalize(rows: Vec<OwnershipRow>) -> BTreeMap<String, CanonicalPerson> {
    let mut people = BTreeMap::new();
    let mut display_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();

    for row in rows {
        let normalized = normalize_name(&row.person_title);
        if normalized.is_empty() {
            continue;
        }

        display_counts
            .entry(normalized.clone())
            .or_default()
            .entry(row.person_title.trim().to_string())
            .and_modify(|count| *count += 1)
            .or_insert(1);

        let entry = people
            .entry(normalized.clone())
            .or_insert_with(|| CanonicalPerson {
                normalized,
                title: row.person_title.trim().to_string(),
                aliases: BTreeSet::new(),
                files: BTreeSet::new(),
            });
        entry.aliases.insert(row.person_path);
        if let Some(file_path) = row.file_path {
            entry.files.insert(file_path);
        }
    }

    for (normalized, person) in &mut people {
        if let Some(counts) = display_counts.get(normalized) {
            if let Some((title, _)) =
                counts
                    .iter()
                    .max_by(|(a_title, a_count), (b_title, b_count)| {
                        a_count.cmp(b_count).then_with(|| b_title.cmp(a_title))
                    })
            {
                person.title = title.clone();
            }
        }
    }

    people
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

async fn count_canonical_edges(pool: &PgPool, edge_type: &str) -> Result<i64> {
    let count = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = $1
           AND (src.path LIKE 'person://canonical/%'
                OR dst.path LIKE 'person://canonical/%')
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
        "#,
    )
    .bind(edge_type)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

fn canonical_path(normalized: &str) -> String {
    format!("person://canonical/{normalized}")
}

fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}
