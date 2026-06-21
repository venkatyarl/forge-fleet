//! Business/domain entity ingestor for Cortex.
//!
//! This derives generic `biz:entity` nodes from existing `db:table` /
//! `db:column` schema nodes. It is intentionally schema-heuristic only: no new
//! storage, no corpus-specific names, and no LLM attribution.

use anyhow::Result;
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

const NODE_TYPE: &str = "biz:entity";
const EDGE_TYPE: &str = "relates_to";
const PROVENANCE: &str = "schema_heuristic";
const CONFIDENCE: f32 = 0.6;

#[derive(Debug, Clone)]
struct TableSchema {
    corpus: String,
    name: String,
    columns: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct Entity {
    corpus: String,
    table: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BizIngestCounts {
    pub entities: i64,
    pub relates_to_edges: i64,
    pub upsert_attempts: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BizEntitySummary {
    pub corpus: String,
    pub title: String,
    pub path: String,
    pub relates_to_count: i64,
}

pub async fn ingest_biz(pool: &PgPool, corpus: Option<&str>) -> Result<BizIngestCounts> {
    let catalog = load_schema_catalog(pool, corpus).await?;
    let entities = domain_entities(&catalog);
    let entity_keys: HashSet<(String, String)> = entities
        .iter()
        .map(|entity| (entity.corpus.clone(), entity.table.clone()))
        .collect();

    let mut upsert_attempts = 0usize;
    for entity in &entities {
        upsert_entity(pool, entity).await?;
        upsert_attempts += 1;
    }

    for entity in &entities {
        let Some(schema) = catalog.get(&(entity.corpus.clone(), entity.table.clone())) else {
            continue;
        };
        for column in &schema.columns {
            let Some(prefix) = column.strip_suffix("_id") else {
                continue;
            };
            let Some(target_table) = resolve_fk_target(prefix, &schema.corpus, &catalog) else {
                continue;
            };
            if target_table == entity.table {
                continue;
            }
            let target_key = (schema.corpus.clone(), target_table.clone());
            if !entity_keys.contains(&target_key) {
                continue;
            }
            if add_edge(
                pool,
                &entity_path(&schema.corpus, &entity.table),
                &entity_path(&schema.corpus, &target_table),
            )
            .await?
            {
                upsert_attempts += 1;
            }
        }
    }

    let mut counts = biz_counts(pool, corpus).await?;
    counts.upsert_attempts = upsert_attempts;
    Ok(counts)
}

pub async fn list_entities(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<BizEntitySummary>> {
    let rows = sqlx::query(
        r#"
        SELECT n.project AS corpus,
               n.title,
               n.path,
               COUNT(e.src_id)::bigint AS relates_to_count
          FROM brain_vault_nodes n
          LEFT JOIN brain_vault_edges e
            ON e.src_id = n.id
           AND e.edge_type = $2
         WHERE n.node_type = $1
           AND n.valid_until IS NULL
           AND ($3::text IS NULL OR n.project = $3)
         GROUP BY n.project, n.title, n.path
         ORDER BY n.project, n.title
        "#,
    )
    .bind(NODE_TYPE)
    .bind(EDGE_TYPE)
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| BizEntitySummary {
            corpus: row.get("corpus"),
            title: row.get("title"),
            path: row.get("path"),
            relates_to_count: row.get("relates_to_count"),
        })
        .collect())
}

async fn load_schema_catalog(
    pool: &PgPool,
    corpus: Option<&str>,
) -> Result<BTreeMap<(String, String), TableSchema>> {
    // Deliberately no current_generation filter: schema nodes may have been
    // written earlier in the same pass before the generation is published.
    let rows = sqlx::query(
        r#"
        SELECT t.project AS corpus,
               t.title AS table_name,
               c.title AS column_title
          FROM brain_vault_nodes t
          LEFT JOIN brain_vault_edges e
            ON e.src_id = t.id
           AND e.edge_type = 'has_column'
          LEFT JOIN brain_vault_nodes c
            ON c.id = e.dst_id
           AND c.node_type = 'db:column'
           AND c.valid_until IS NULL
         WHERE t.node_type = 'db:table'
           AND t.valid_until IS NULL
           AND ($1::text IS NULL OR t.project = $1)
         ORDER BY t.project, t.title, c.title
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    let mut catalog: BTreeMap<(String, String), TableSchema> = BTreeMap::new();
    for row in rows {
        let corpus: String = row.get("corpus");
        let table = norm_ident(row.get::<String, _>("table_name").as_str());
        let key = (corpus.clone(), table.clone());
        let schema = catalog.entry(key).or_insert_with(|| TableSchema {
            corpus,
            name: table,
            columns: BTreeSet::new(),
        });
        if let Some(column_title) = row.try_get::<Option<String>, _>("column_title")? {
            let column = column_title
                .rsplit_once('.')
                .map(|(_, column)| column)
                .unwrap_or(&column_title);
            schema.columns.insert(norm_ident(column));
        }
    }
    Ok(catalog)
}

fn domain_entities(catalog: &BTreeMap<(String, String), TableSchema>) -> Vec<Entity> {
    catalog
        .values()
        .filter(|table| is_domain_entity(table))
        .map(|table| Entity {
            corpus: table.corpus.clone(),
            table: table.name.clone(),
        })
        .collect()
}

fn is_domain_entity(table: &TableSchema) -> bool {
    if is_infra_table(&table.name) || is_junction_table(table) {
        return false;
    }
    let has_id_pk = table.columns.contains("id") || table.columns.contains("uuid");
    let non_fk_columns = table
        .columns
        .iter()
        .filter(|column| {
            !column.ends_with("_id") && column.as_str() != "id" && column.as_str() != "uuid"
        })
        .count();
    has_id_pk && non_fk_columns >= 2
}

fn is_infra_table(name: &str) -> bool {
    name == "_migrations"
        || name.ends_with("_migrations")
        || name.ends_with("_log")
        || name.ends_with("_audit")
        || name.ends_with("_jobs")
        || name.ends_with("_cache")
        || name.ends_with("_queue")
}

fn is_junction_table(table: &TableSchema) -> bool {
    let fk_columns = table
        .columns
        .iter()
        .filter(|column| column.ends_with("_id"))
        .count();
    let little_else = table.columns.len().saturating_sub(fk_columns) <= 2;
    fk_columns == 2 && little_else
}

fn resolve_fk_target(
    prefix: &str,
    corpus: &str,
    catalog: &BTreeMap<(String, String), TableSchema>,
) -> Option<String> {
    for candidate in plural_candidates(prefix) {
        if catalog.contains_key(&(corpus.to_string(), candidate.clone())) {
            return Some(candidate);
        }
    }

    let tables: HashMap<String, String> = catalog
        .keys()
        .filter(|(table_corpus, _)| table_corpus == corpus)
        .map(|(_, table)| (singularize(table), table.clone()))
        .collect();
    tables.get(prefix).cloned()
}

fn plural_candidates(prefix: &str) -> Vec<String> {
    let mut out = vec![
        prefix.to_string(),
        format!("{prefix}s"),
        format!("{prefix}es"),
    ];
    if let Some(stem) = prefix.strip_suffix('y') {
        out.push(format!("{stem}ies"));
    }
    out
}

fn singularize(name: &str) -> String {
    if let Some(stem) = name.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if let Some(stem) = name.strip_suffix("es") {
        return stem.to_string();
    }
    if let Some(stem) = name.strip_suffix('s') {
        return stem.to_string();
    }
    name.to_string()
}

async fn upsert_entity(pool: &PgPool, entity: &Entity) -> Result<Uuid> {
    let path = entity_path(&entity.corpus, &entity.table);
    let id = sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_nodes
            (path, title, node_type, project, content_hash, confidence, provenance)
        VALUES ($1, $2, $3, $4, $1, $5, $6)
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
    .bind(&entity.table)
    .bind(NODE_TYPE)
    .bind(&entity.corpus)
    .bind(CONFIDENCE)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn add_edge(pool: &PgPool, src_path: &str, dst_path: &str) -> Result<bool> {
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
        SELECT src_id, dst_id, $3, $4, $5, 'TABLE'
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
    .bind(EDGE_TYPE)
    .bind(PROVENANCE)
    .bind(CONFIDENCE)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.unwrap_or(false))
}

async fn biz_counts(pool: &PgPool, corpus: Option<&str>) -> Result<BizIngestCounts> {
    let entities = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_nodes
         WHERE node_type = $1
           AND valid_until IS NULL
           AND ($2::text IS NULL OR project = $2)
        "#,
    )
    .bind(NODE_TYPE)
    .bind(corpus)
    .fetch_one(pool)
    .await?;

    let relates_to_edges = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
          FROM brain_vault_edges e
          JOIN brain_vault_nodes src ON src.id = e.src_id
          JOIN brain_vault_nodes dst ON dst.id = e.dst_id
         WHERE e.edge_type = $1
           AND src.node_type = $2
           AND dst.node_type = $2
           AND src.valid_until IS NULL
           AND dst.valid_until IS NULL
           AND ($3::text IS NULL OR src.project = $3)
        "#,
    )
    .bind(EDGE_TYPE)
    .bind(NODE_TYPE)
    .bind(corpus)
    .fetch_one(pool)
    .await?;

    Ok(BizIngestCounts {
        entities,
        relates_to_edges,
        upsert_attempts: 0,
    })
}

fn entity_path(corpus: &str, table: &str) -> String {
    format!("biz://{corpus}/{table}")
}

fn norm_ident(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .rsplit_once('.')
        .map(|(_, tail)| tail)
        .unwrap_or(value)
        .to_ascii_lowercase()
}
