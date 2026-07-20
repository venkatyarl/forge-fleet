//! Storage backend abstraction for Cortex.
//!
//! Postgres remains the production write backend. Cortex reads can opt into a
//! native FalkorDB graph, with transparent per-operation Postgres fallback.

use anyhow::{Context, Result};
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::OnceCell;
use uuid::Uuid;

const FALKOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

use super::{
    CommunityExplanation, FileOutline, OutlineEntry, SymbolHit, SymbolRef, TestHit,
    add_edge_with_metadata, call_path_postgres, callees_postgres, callers_postgres,
    explain_community_postgres, find_symbols_postgres, find_symbols_semantic_postgres,
    impact_postgres, is_test_symbol, outline_file_postgres, resolve_kind_filter,
    tests_for_postgres, upsert_code_node,
};

/// A Cortex node as stored in the backend-neutral graph contract.
#[derive(Debug, Clone)]
pub struct CortexGraphNode {
    pub path: String,
    pub title: String,
    pub node_type: String,
    pub project: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub generation: i64,
    pub confidence: f32,
    pub provenance: String,
}

/// A Cortex edge as stored in the backend-neutral graph contract.
#[derive(Debug, Clone)]
pub struct CortexGraphEdge {
    pub src_id: Uuid,
    pub dst_id: Uuid,
    pub edge_type: String,
    pub generation: i64,
    pub confidence: f32,
    pub provenance: String,
    pub method: Option<String>,
    pub evidence: Option<serde_json::Value>,
}

/// Storage operations Cortex needs from any graph backend.
///
/// The boundary intentionally mirrors the current behavior: idempotent node and
/// edge writes for extraction, embedding persistence for semantic search, and
/// graph traversals for the MCP/CLI read surface. Auxiliary Postgres-only
/// ledgers (`cortex_file_index`, `cortex_reexports`, `cortex_generations`) stay
/// outside this trait until the dual-write phase proves the graph data path.
#[async_trait]
pub trait CortexGraphStore: Send + Sync {
    fn backend_name(&self) -> &'static str;

    async fn upsert_node(&self, node: &CortexGraphNode) -> Result<Uuid>;
    async fn add_edge(&self, edge: &CortexGraphEdge) -> Result<bool>;
    async fn wipe_code_nodes(&self, corpus_slug: &str) -> Result<()>;
    async fn store_embedding(&self, node_id: Uuid, embedding: &[f32]) -> Result<()>;

    async fn find_symbols(
        &self,
        corpus_slug: &str,
        query: &str,
        limit: i64,
        kind: Option<&str>,
        semantic: bool,
    ) -> Result<Vec<SymbolHit>>;
    async fn callers(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>>;
    async fn callees(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>>;
    async fn impact(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>>;
    async fn call_path(
        &self,
        corpus_slug: &str,
        from: &str,
        to: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>>;
    async fn tests_for(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<TestHit>>;
    async fn explain_community(
        &self,
        corpus_slug: &str,
        symbol: &str,
        kind: Option<&str>,
        member_limit: i64,
    ) -> Result<Option<CommunityExplanation>>;
    async fn outline_file(
        &self,
        corpus_slug: &str,
        file: &str,
        kind: Option<&str>,
    ) -> Result<Option<FileOutline>>;
}

/// Existing Postgres/pgvector implementation. This delegates to the current
/// battle-tested helpers so behavior does not change while the trait lands.
#[derive(Clone)]
pub struct PostgresCortexGraphStore {
    pool: PgPool,
}

impl PostgresCortexGraphStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CortexGraphStore for PostgresCortexGraphStore {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }

    async fn upsert_node(&self, node: &CortexGraphNode) -> Result<Uuid> {
        upsert_code_node(
            &self.pool,
            &node.path,
            &node.title,
            &node.node_type,
            &node.project,
            node.start_line,
            node.end_line,
            node.generation,
            node.confidence,
            &node.provenance,
        )
        .await
    }

    async fn add_edge(&self, edge: &CortexGraphEdge) -> Result<bool> {
        add_edge_with_metadata(
            &self.pool,
            edge.src_id,
            edge.dst_id,
            &edge.edge_type,
            edge.confidence,
            &edge.provenance,
            edge.method.as_deref(),
            edge.evidence.as_ref(),
            edge.generation,
        )
        .await
    }

    async fn wipe_code_nodes(&self, corpus_slug: &str) -> Result<()> {
        super::wipe_code_nodes(&self.pool, corpus_slug).await
    }

    async fn store_embedding(&self, node_id: Uuid, embedding: &[f32]) -> Result<()> {
        let pgvec = crate::vector_search::embedding_to_pgvector(embedding);
        sqlx::query(
            "UPDATE brain_vault_nodes SET embedding = $1::vector, updated_at = NOW() WHERE id = $2",
        )
        .bind(&pgvec)
        .bind(node_id)
        .execute(&self.pool)
        .await?;
        mirror_embedding(&self.pool, node_id, embedding).await;
        Ok(())
    }

    async fn find_symbols(
        &self,
        corpus_slug: &str,
        query: &str,
        limit: i64,
        kind: Option<&str>,
        semantic: bool,
    ) -> Result<Vec<SymbolHit>> {
        if semantic {
            find_symbols_semantic_postgres(&self.pool, corpus_slug, query, limit, kind).await
        } else {
            find_symbols_postgres(&self.pool, corpus_slug, query, limit, kind).await
        }
    }

    async fn callers(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        callers_postgres(&self.pool, corpus_slug, symbol, min_confidence).await
    }

    async fn callees(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        callees_postgres(&self.pool, corpus_slug, symbol, min_confidence).await
    }

    async fn impact(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        impact_postgres(&self.pool, corpus_slug, symbol, max_depth, min_confidence).await
    }

    async fn call_path(
        &self,
        corpus_slug: &str,
        from: &str,
        to: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>> {
        call_path_postgres(&self.pool, corpus_slug, from, to, max_depth, min_confidence).await
    }

    async fn tests_for(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<TestHit>> {
        tests_for_postgres(&self.pool, corpus_slug, symbol, max_depth, min_confidence).await
    }

    async fn explain_community(
        &self,
        corpus_slug: &str,
        symbol: &str,
        kind: Option<&str>,
        member_limit: i64,
    ) -> Result<Option<CommunityExplanation>> {
        explain_community_postgres(&self.pool, corpus_slug, symbol, kind, member_limit).await
    }

    async fn outline_file(
        &self,
        corpus_slug: &str,
        file: &str,
        kind: Option<&str>,
    ) -> Result<Option<FileOutline>> {
        outline_file_postgres(&self.pool, corpus_slug, file, kind).await
    }
}

/// FalkorDB/OpenCypher implementation. Reads use `GRAPH.RO_QUERY`; opt-in
/// best-effort mirrors use `GRAPH.QUERY` after the authoritative Postgres write.
#[derive(Clone)]
pub struct FalkorCortexGraphStore {
    connection: ConnectionManager,
    graph_name: String,
    pool: PgPool,
}

impl FalkorCortexGraphStore {
    pub fn new(connection: ConnectionManager, graph_name: impl Into<String>, pool: PgPool) -> Self {
        Self {
            connection,
            graph_name: graph_name.into(),
            pool,
        }
    }

    async fn graph_query(&self, cypher: &str) -> Result<redis::Value> {
        let mut conn = self.connection.clone();
        let value = tokio::time::timeout(
            FALKOR_TIMEOUT,
            redis::cmd("GRAPH.QUERY")
                .arg(&self.graph_name)
                .arg(cypher)
                .arg("TIMEOUT")
                .arg(FALKOR_TIMEOUT.as_millis() as u64)
                .query_async(&mut conn),
        )
        .await
        .context("FalkorDB write timed out")??;
        Ok(value)
    }

    async fn upsert_node_with_id(&self, id: Uuid, node: &CortexGraphNode) -> Result<()> {
        let query = format!(
            "MERGE (n:CortexNode {{path: {path}}}) \
             SET n.id = {id}, n.title = {title}, n.node_type = {node_type}, \
                 n.project = {project}, n.start_line = {start_line}, \
                 n.end_line = {end_line}, n.generation = {generation}, \
                 n.confidence = {confidence}, n.provenance = {provenance}, \
                 n.valid_until = NULL",
            path = cypher_string(&node.path),
            id = cypher_string(&id.to_string()),
            title = cypher_string(&node.title),
            node_type = cypher_string(&node.node_type),
            project = cypher_string(&node.project),
            start_line = cypher_optional_i32(node.start_line),
            end_line = cypher_optional_i32(node.end_line),
            generation = node.generation,
            confidence = finite_f32(node.confidence)?,
            provenance = cypher_string(&node.provenance),
        );
        self.graph_query(&query).await?;
        Ok(())
    }

    async fn graph_read(&self, cypher: &str) -> Result<Vec<Vec<redis::Value>>> {
        let mut conn = self.connection.clone();
        let value = tokio::time::timeout(
            FALKOR_TIMEOUT,
            redis::cmd("GRAPH.RO_QUERY")
                .arg(&self.graph_name)
                .arg(cypher)
                .arg("TIMEOUT")
                .arg(FALKOR_TIMEOUT.as_millis() as u64)
                .query_async(&mut conn),
        )
        .await
        .context("FalkorDB read timed out")??;
        decode_rows(value)
    }

    /// Create the indexes expected by the FalkorDB backend.
    pub async fn ensure_schema(&self) -> Result<()> {
        for query in [
            FALKOR_CREATE_PATH_INDEX,
            FALKOR_CREATE_PROJECT_INDEX,
            FALKOR_CREATE_TITLE_INDEX,
            FALKOR_CREATE_VECTOR_INDEX,
        ] {
            self.graph_query(query).await?;
        }
        Ok(())
    }
}

const FALKOR_CREATE_PATH_INDEX: &str = "CREATE INDEX FOR (n:CortexNode) ON (n.path)";
const FALKOR_CREATE_PROJECT_INDEX: &str = "CREATE INDEX FOR (n:CortexNode) ON (n.project)";
const FALKOR_CREATE_TITLE_INDEX: &str = "CREATE FULLTEXT INDEX ON :CortexNode(title)";
const FALKOR_CREATE_VECTOR_INDEX: &str = "CREATE VECTOR INDEX FOR (n:CortexNode) ON (n.embedding) OPTIONS {dimension: 1024, similarityFunction: 'cosine'}";

const FALKOR_FIND_SYMBOLS_CYPHER: &str = r#"
MATCH (n:CortexNode)
WHERE n.project = $project
  AND n.node_type STARTS WITH 'code:'
  AND toLower(n.title) CONTAINS toLower($query)
RETURN n.id, n.title, n.node_type, n.start_line,
       size((:CortexNode)-[:calls]->(n)) AS fan_in
ORDER BY fan_in DESC, n.title
LIMIT $limit
"#;

const FALKOR_SEMANTIC_SEARCH_CYPHER: &str = r#"
CALL db.idx.vector.queryNodes('CortexNode', 'embedding', $limit, vecf32($embedding))
YIELD node, score
WHERE node.project = $project AND node.node_type STARTS WITH 'code:'
RETURN node.id, node.title, node.node_type, node.start_line, score
ORDER BY score DESC
"#;

const FALKOR_CALLERS_CYPHER: &str = r#"
MATCH (target:CortexNode)<-[e:calls]-(caller:CortexNode)
WHERE target.project = $project
  AND (target.path = $path OR target.title = $symbol OR target.title ENDS WITH $leaf_suffix)
  AND e.confidence >= $min_confidence
RETURN DISTINCT caller.id, caller.title, caller.node_type, caller.start_line
ORDER BY caller.title
"#;

const FALKOR_CALLEES_CYPHER: &str = r#"
MATCH (source:CortexNode)-[e:calls]->(callee:CortexNode)
WHERE source.project = $project
  AND (source.path = $path OR source.title = $symbol OR source.title ENDS WITH $leaf_suffix)
  AND e.confidence >= $min_confidence
RETURN DISTINCT callee.id, callee.title, callee.node_type, callee.start_line
ORDER BY callee.title
"#;

const FALKOR_IMPACT_CYPHER: &str = r#"
MATCH (target:CortexNode)<-[path:calls*1..$max_depth]-(caller:CortexNode)
WHERE target.project = $project
  AND (target.path = $path OR target.title = $symbol OR target.title ENDS WITH $leaf_suffix)
  AND all(edge IN relationships(path) WHERE edge.confidence >= $min_confidence)
RETURN DISTINCT caller.id, caller.title, caller.node_type, caller.start_line
ORDER BY caller.title
"#;

const FALKOR_CALL_PATH_CYPHER: &str = r#"
MATCH p = shortestPath((source:CortexNode)-[:calls*1..$max_depth]->(target:CortexNode))
WHERE source.project = $project
  AND (source.path = $from_path OR source.title = $from OR source.title ENDS WITH $from_leaf_suffix)
  AND (target.path = $to_path OR target.title = $to OR target.title ENDS WITH $to_leaf_suffix)
RETURN nodes(p)
"#;

const FALKOR_OUTLINE_FILE_CYPHER: &str = r#"
MATCH (file:CortexNode)-[:contains*1..]->(symbol:CortexNode)
WHERE file.project = $project
  AND file.node_type = 'content:file'
  AND (file.path = $file OR file.path ENDS WITH $file_suffix)
  AND symbol.node_type STARTS WITH 'code:'
RETURN symbol.title, symbol.node_type, symbol.start_line, symbol.end_line,
       size((:CortexNode)-[:calls]->(symbol)) AS fan_in
ORDER BY symbol.start_line, symbol.title
"#;

fn cypher_string(value: &str) -> String {
    format!(
        "'{}'",
        value
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

fn cypher_optional_i32(value: Option<i32>) -> String {
    value.map_or_else(|| "NULL".to_string(), |value| value.to_string())
}

fn stable_uuid_from_path(path: &str) -> Uuid {
    let hash = Sha256::digest(path.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    Uuid::from_bytes(bytes)
}

fn finite_f32(value: f32) -> Result<String> {
    if !value.is_finite() {
        anyhow::bail!("non-finite float cannot be written to FalkorDB");
    }
    Ok(value.to_string())
}

fn cypher_relationship_type(value: &str) -> Result<&str> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && value.as_bytes()[0].is_ascii_alphabetic()
    {
        Ok(value)
    } else {
        anyhow::bail!("invalid FalkorDB relationship type {value:?}")
    }
}

fn decode_rows(value: redis::Value) -> Result<Vec<Vec<redis::Value>>> {
    let redis::Value::Array(mut response) = value else {
        anyhow::bail!("unexpected FalkorDB response: expected top-level array");
    };
    if response.len() < 2 {
        return Ok(Vec::new());
    }
    let redis::Value::Array(rows) = response.swap_remove(1) else {
        anyhow::bail!("unexpected FalkorDB response: expected result rows");
    };
    rows.into_iter()
        .map(|row| match row {
            redis::Value::Array(values) => Ok(values),
            _ => anyhow::bail!("unexpected FalkorDB response: row is not an array"),
        })
        .collect()
}

fn value_string(value: &redis::Value) -> Result<String> {
    match value {
        redis::Value::BulkString(bytes) => {
            String::from_utf8(bytes.clone()).context("FalkorDB returned non-UTF8 text")
        }
        redis::Value::SimpleString(value) => Ok(value.clone()),
        other => anyhow::bail!("unexpected FalkorDB string value: {other:?}"),
    }
}

fn value_optional_string(value: &redis::Value) -> Result<Option<String>> {
    if matches!(value, redis::Value::Nil) {
        Ok(None)
    } else {
        value_string(value).map(Some)
    }
}

fn value_i64(value: &redis::Value) -> Result<i64> {
    match value {
        redis::Value::Int(value) => Ok(*value),
        redis::Value::Double(value) => Ok(*value as i64),
        _ => value_string(value)?
            .parse()
            .context("invalid FalkorDB integer"),
    }
}

fn value_optional_i32(value: &redis::Value) -> Result<Option<i32>> {
    if matches!(value, redis::Value::Nil) {
        Ok(None)
    } else {
        Ok(Some(
            i32::try_from(value_i64(value)?).context("FalkorDB integer exceeds i32")?,
        ))
    }
}

fn value_f32(value: &redis::Value) -> Result<f32> {
    match value {
        redis::Value::Double(value) => Ok(*value as f32),
        redis::Value::Int(value) => Ok(*value as f32),
        _ => value_string(value)?
            .parse()
            .context("invalid FalkorDB float"),
    }
}

fn value_uuid(value: &redis::Value) -> Result<Uuid> {
    Uuid::parse_str(&value_string(value)?).context("invalid FalkorDB node UUID")
}

fn symbol_ref(row: &[redis::Value]) -> Result<SymbolRef> {
    if row.len() < 5 {
        anyhow::bail!("FalkorDB symbol row has {} columns, expected 5", row.len());
    }
    Ok(SymbolRef {
        id: value_uuid(&row[0])?,
        qualified_name: value_string(&row[1])?,
        node_type: value_string(&row[2])?,
        start_line: value_optional_i32(&row[3])?,
        file: value_optional_string(&row[4])?,
    })
}

fn selector_predicate(alias: &str, corpus: &str, symbol: &str) -> String {
    let path = format!("code://{corpus}/{symbol}");
    let suffix = format!("%::{symbol}");
    format!(
        "{alias}.project = {project} AND ({alias}.path = {path} OR {alias}.title = {symbol} OR {alias}.title ENDS WITH {suffix})",
        project = cypher_string(corpus),
        path = cypher_string(&path),
        symbol = cypher_string(symbol),
        suffix = cypher_string(suffix.trim_start_matches('%')),
    )
}

#[async_trait]
impl CortexGraphStore for FalkorCortexGraphStore {
    fn backend_name(&self) -> &'static str {
        "falkordb"
    }

    async fn upsert_node(&self, node: &CortexGraphNode) -> Result<Uuid> {
        let id = stable_uuid_from_path(&node.path);
        self.upsert_node_with_id(id, node).await?;
        Ok(id)
    }

    async fn add_edge(&self, edge: &CortexGraphEdge) -> Result<bool> {
        let edge_type = cypher_relationship_type(&edge.edge_type)?;
        let query = format!(
            "MATCH (s:CortexNode {{id: {src}}}), (d:CortexNode {{id: {dst}}}) \
             MERGE (s)-[e:{edge_type}]->(d) \
             SET e.confidence = {confidence}, e.provenance = {provenance}, \
                 e.method = {method}, e.evidence = {evidence}, e.generation = {generation} \
             RETURN id(e)",
            src = cypher_string(&edge.src_id.to_string()),
            dst = cypher_string(&edge.dst_id.to_string()),
            edge_type = edge_type,
            confidence = finite_f32(edge.confidence)?,
            provenance = cypher_string(&edge.provenance),
            method = edge
                .method
                .as_deref()
                .map(cypher_string)
                .unwrap_or_else(|| "NULL".to_string()),
            evidence = edge
                .evidence
                .as_ref()
                .map(|v| cypher_string(&v.to_string()))
                .unwrap_or_else(|| "NULL".to_string()),
            generation = edge.generation,
        );
        self.graph_query(&query).await?;
        Ok(true)
    }

    async fn wipe_code_nodes(&self, corpus_slug: &str) -> Result<()> {
        let query = format!(
            "MATCH (n:CortexNode) WHERE n.project = {} AND n.node_type STARTS WITH 'code:' DETACH DELETE n",
            cypher_string(corpus_slug)
        );
        self.graph_query(&query).await?;
        Ok(())
    }

    async fn store_embedding(&self, node_id: Uuid, embedding: &[f32]) -> Result<()> {
        let vector = embedding
            .iter()
            .map(|value| finite_f32(*value))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let query = format!(
            "MATCH (n:CortexNode {{id: {}}}) SET n.embedding = vecf32([{}]) RETURN n.id",
            cypher_string(&node_id.to_string()),
            vector
        );
        self.graph_query(&query).await?;
        Ok(())
    }

    async fn find_symbols(
        &self,
        corpus_slug: &str,
        query: &str,
        limit: i64,
        kind: Option<&str>,
        semantic: bool,
    ) -> Result<Vec<SymbolHit>> {
        let limit = limit.clamp(1, 500);
        let kind_types = resolve_kind_filter(kind)?;
        let kind_clause = kind_types
            .map(|types| {
                format!(
                    " AND n.node_type IN [{}]",
                    types
                        .into_iter()
                        .map(|value| cypher_string(&value))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .unwrap_or_default();
        let (match_clause, score_expr) = if semantic {
            let client = crate::embeddings::fleet_embedding_client(&self.pool)
                .await
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no healthy fleet embedding endpoint for FalkorDB semantic search"
                    )
                })?;
            let embedding = client
                .embed(query)
                .await
                .map_err(|e| anyhow::anyhow!("embed query: {e}"))?;
            let vector = embedding
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",");
            (
                format!(
                    "CALL db.idx.vector.queryNodes('CortexNode', 'embedding', {limit}, vecf32([{vector}])) YIELD node AS n, score"
                ),
                "score".to_string(),
            )
        } else {
            (
                format!(
                    "MATCH (n:CortexNode) WHERE toLower(n.title) CONTAINS toLower({})",
                    cypher_string(query)
                ),
                "NULL".to_string(),
            )
        };
        let connector = if semantic { "WHERE" } else { "AND" };
        let cypher = format!(
            "{match_clause} {connector} n.project = {project} AND n.node_type STARTS WITH 'code:'{kind_clause} \
             OPTIONAL MATCH (file:CortexNode {{node_type: 'content:file'}})-[:contains*1..]->(n) \
             OPTIONAL MATCH (:CortexNode)-[incoming:calls]->(n) \
             RETURN n.id, n.title, n.node_type, n.start_line, file.path, count(DISTINCT incoming), {score_expr} \
             ORDER BY {score_expr} DESC, count(DISTINCT incoming) DESC, n.title LIMIT {limit}",
            project = cypher_string(corpus_slug),
        );
        self.graph_read(&cypher)
            .await?
            .into_iter()
            .map(|row| {
                if row.len() < 7 {
                    anyhow::bail!("FalkorDB find row has {} columns, expected 7", row.len());
                }
                Ok(SymbolHit {
                    id: value_uuid(&row[0])?,
                    qualified_name: value_string(&row[1])?,
                    node_type: value_string(&row[2])?,
                    start_line: value_optional_i32(&row[3])?,
                    file: value_optional_string(&row[4])?,
                    fan_in: value_i64(&row[5])?,
                    score: if semantic {
                        Some(value_f32(&row[6])?)
                    } else {
                        None
                    },
                })
            })
            .collect()
    }

    async fn callers(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        let cypher = format!(
            "MATCH (target:CortexNode)<-[e:calls]-(n:CortexNode) WHERE {} AND e.confidence >= {} OPTIONAL MATCH (file:CortexNode {{node_type: 'content:file'}})-[:contains*1..]->(n) RETURN DISTINCT n.id, n.title, n.node_type, n.start_line, file.path ORDER BY n.title",
            selector_predicate("target", corpus_slug, symbol),
            min_confidence
        );
        self.graph_read(&cypher)
            .await?
            .iter()
            .map(|row| symbol_ref(row))
            .collect()
    }

    async fn callees(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        let cypher = format!(
            "MATCH (source:CortexNode)-[e:calls]->(n:CortexNode) WHERE {} AND e.confidence >= {} OPTIONAL MATCH (file:CortexNode {{node_type: 'content:file'}})-[:contains*1..]->(n) RETURN DISTINCT n.id, n.title, n.node_type, n.start_line, file.path ORDER BY n.title",
            selector_predicate("source", corpus_slug, symbol),
            min_confidence
        );
        self.graph_read(&cypher)
            .await?
            .iter()
            .map(|row| symbol_ref(row))
            .collect()
    }

    async fn impact(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        let depth = max_depth.clamp(1, 20);
        let cypher = format!(
            "MATCH p=(target:CortexNode)<-[:calls*1..{depth}]-(n:CortexNode) WHERE {} AND all(e IN relationships(p) WHERE e.confidence >= {}) OPTIONAL MATCH (file:CortexNode {{node_type: 'content:file'}})-[:contains*1..]->(n) RETURN DISTINCT n.id, n.title, n.node_type, n.start_line, file.path ORDER BY n.title",
            selector_predicate("target", corpus_slug, symbol),
            min_confidence
        );
        self.graph_read(&cypher)
            .await?
            .iter()
            .map(|row| symbol_ref(row))
            .collect()
    }

    async fn call_path(
        &self,
        corpus_slug: &str,
        from: &str,
        to: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>> {
        let depth = max_depth.clamp(1, 20);
        let cypher = format!(
            "MATCH p=shortestPath((source:CortexNode)-[:calls*1..{depth}]->(target:CortexNode)) WHERE {} AND {} AND all(e IN relationships(p) WHERE e.confidence >= {}) UNWIND nodes(p) AS n OPTIONAL MATCH (file:CortexNode {{node_type: 'content:file'}})-[:contains*1..]->(n) RETURN n.id, n.title, n.node_type, n.start_line, file.path",
            selector_predicate("source", corpus_slug, from),
            selector_predicate("target", corpus_slug, to),
            min_confidence
        );
        let rows = self.graph_read(&cypher).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            rows.iter()
                .map(|row| symbol_ref(row))
                .collect::<Result<_>>()?,
        ))
    }

    async fn tests_for(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<TestHit>> {
        let depth = max_depth.clamp(1, 20);
        let cypher = format!(
            "MATCH p=(target:CortexNode)<-[:calls*1..{depth}]-(n:CortexNode) WHERE {} AND n.node_type = 'code:function' AND all(e IN relationships(p) WHERE e.confidence >= {}) OPTIONAL MATCH (file:CortexNode {{node_type: 'content:file'}})-[:contains*1..]->(n) RETURN n.id, n.title, n.start_line, file.path, min(length(p)) ORDER BY min(length(p)), n.title",
            selector_predicate("target", corpus_slug, symbol),
            min_confidence
        );
        let mut out = self
            .graph_read(&cypher)
            .await?
            .into_iter()
            .filter_map(|row| {
                let parsed = (|| -> Result<TestHit> {
                    if row.len() < 5 {
                        anyhow::bail!("FalkorDB test row has {} columns, expected 5", row.len());
                    }
                    Ok(TestHit {
                        id: value_uuid(&row[0])?,
                        qualified_name: value_string(&row[1])?,
                        start_line: value_optional_i32(&row[2])?,
                        file: value_optional_string(&row[3])?,
                        depth: usize::try_from(value_i64(&row[4])?)
                            .context("invalid FalkorDB path depth")?,
                    })
                })();
                match parsed {
                    Ok(hit) if is_test_symbol(&hit.qualified_name, hit.file.as_deref()) => {
                        Some(Ok(hit))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then(a.qualified_name.cmp(&b.qualified_name))
        });
        Ok(out)
    }

    async fn explain_community(
        &self,
        _corpus_slug: &str,
        _symbol: &str,
        _kind: Option<&str>,
        _member_limit: i64,
    ) -> Result<Option<CommunityExplanation>> {
        anyhow::bail!("FalkorDB community explanation waits on code-community persistence/backfill")
    }

    async fn outline_file(
        &self,
        corpus_slug: &str,
        file: &str,
        kind: Option<&str>,
    ) -> Result<Option<FileOutline>> {
        let kind_types = resolve_kind_filter(kind)?;
        let kind_clause = kind_types
            .map(|types| {
                format!(
                    " AND n.node_type IN [{}]",
                    types
                        .into_iter()
                        .map(|value| cypher_string(&value))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .unwrap_or_default();
        let suffix = format!("/{file}");
        let cypher = format!(
            "MATCH (f:CortexNode)-[:contains*1..]->(n:CortexNode) WHERE f.project = {} AND f.node_type = 'content:file' AND (f.path = {} OR f.path ENDS WITH {}) AND n.node_type STARTS WITH 'code:'{} OPTIONAL MATCH (:CortexNode)-[incoming:calls]->(n) RETURN f.path, n.title, n.node_type, n.start_line, n.end_line, count(DISTINCT incoming) ORDER BY n.start_line, n.title",
            cypher_string(corpus_slug),
            cypher_string(file),
            cypher_string(&suffix),
            kind_clause
        );
        let rows = self.graph_read(&cypher).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let paths = rows
            .iter()
            .map(|row| value_string(&row[0]))
            .collect::<Result<Vec<_>>>()?;
        let first = &paths[0];
        if paths.iter().any(|path| path != first) {
            anyhow::bail!(
                "'{file}' matches multiple files in corpus '{corpus_slug}' — pass more of the path to disambiguate"
            );
        }
        let symbols = rows
            .into_iter()
            .map(|row| {
                if row.len() < 6 {
                    anyhow::bail!("FalkorDB outline row has {} columns, expected 6", row.len());
                }
                Ok(OutlineEntry {
                    qualified_name: value_string(&row[1])?,
                    node_type: value_string(&row[2])?,
                    start_line: value_optional_i32(&row[3])?,
                    end_line: value_optional_i32(&row[4])?,
                    fan_in: value_i64(&row[5])?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Some(FileOutline {
            file: first.clone(),
            symbols,
        }))
    }
}

static FALKOR_WRITE_STORE: OnceCell<Option<FalkorCortexGraphStore>> = OnceCell::const_new();

async fn fleet_setting(pool: &PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT value FROM fleet_secrets WHERE key = $1 AND disabled_reason IS NULL",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

fn enabled_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "falkordb"
    )
}

async fn falkor_write_store(pool: &PgPool) -> Option<&'static FalkorCortexGraphStore> {
    FALKOR_WRITE_STORE
        .get_or_init(|| async {
            let backend = std::env::var("CORTEX_GRAPH_BACKEND").ok();
            let dual_write = std::env::var("CORTEX_DUAL_WRITE").ok();
            let enabled = backend.as_deref() == Some("falkordb")
                || dual_write.as_deref().is_some_and(enabled_value)
                || fleet_setting(pool, "cortex.graph_backend").await.as_deref()
                    == Some("falkordb")
                || fleet_setting(pool, "cortex.dual_write")
                    .await
                    .as_deref()
                    .is_some_and(enabled_value);
            if !enabled {
                return None;
            }

            let result = async {
                let url = match std::env::var("FALKORDB_URL") {
                    Ok(url) => url,
                    Err(_) => fleet_setting(pool, "falkordb.url")
                        .await
                        .context("FalkorDB dual-write requires FALKORDB_URL or fleet_secrets falkordb.url")?,
                };
                let graph = std::env::var("FALKORDB_GRAPH")
                    .ok()
                    .or(fleet_setting(pool, "falkordb.graph").await)
                    .unwrap_or_else(|| "cortex".to_string());
                let client = redis::Client::open(url).context("open FalkorDB Redis client")?;
                let connection = tokio::time::timeout(FALKOR_TIMEOUT, ConnectionManager::new(client))
                    .await
                    .context("FalkorDB connection timed out")?
                    .context("connect to FalkorDB")?;
                Ok::<_, anyhow::Error>(FalkorCortexGraphStore::new(
                    connection,
                    graph,
                    pool.clone(),
                ))
            }
            .await;
            match result {
                Ok(store) => Some(store),
                Err(error) => {
                    tracing::warn!(%error, "FalkorDB Cortex dual-write unavailable; Postgres remains authoritative");
                    None
                }
            }
        })
        .await
        .as_ref()
}

async fn best_effort_write<F>(pool: &PgPool, operation: &'static str, write: F)
where
    F: for<'a> FnOnce(
        &'a FalkorCortexGraphStore,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>,
    >,
{
    let Some(store) = falkor_write_store(pool).await else {
        return;
    };
    if let Err(error) = write(store).await {
        tracing::warn!(%error, operation, "FalkorDB Cortex dual-write failed; Postgres write retained");
    }
}

pub(super) async fn mirror_node_upsert(pool: &PgPool, id: Uuid, node: CortexGraphNode) {
    best_effort_write(pool, "upsert_node", move |store| {
        Box::pin(async move { store.upsert_node_with_id(id, &node).await })
    })
    .await;
}

pub(super) async fn mirror_edge_upsert(pool: &PgPool, edge: CortexGraphEdge) {
    best_effort_write(pool, "add_edge", move |store| {
        Box::pin(async move { store.add_edge(&edge).await.map(|_| ()) })
    })
    .await;
}

pub(super) async fn mirror_wipe_code_nodes(pool: &PgPool, corpus_slug: &str) {
    let corpus_slug = corpus_slug.to_string();
    best_effort_write(pool, "wipe_code_nodes", move |store| {
        Box::pin(async move { store.wipe_code_nodes(&corpus_slug).await })
    })
    .await;
}

fn uuid_list(ids: &[Uuid]) -> String {
    ids.iter()
        .map(|id| cypher_string(&id.to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) async fn mirror_delete_nodes(pool: &PgPool, ids: &[Uuid]) {
    if ids.is_empty() {
        return;
    }
    let ids = uuid_list(ids);
    best_effort_write(pool, "delete_nodes", move |store| {
        Box::pin(async move {
            store
                .graph_query(&format!(
                    "MATCH (n:CortexNode) WHERE n.id IN [{ids}] DETACH DELETE n"
                ))
                .await?;
            Ok(())
        })
    })
    .await;
}

pub(super) async fn mirror_delete_outgoing_edges(pool: &PgPool, ids: &[Uuid]) {
    mirror_delete_edges(pool, ids, &["calls", "contains", "imports"]).await;
}

pub(super) async fn mirror_delete_edges(pool: &PgPool, ids: &[Uuid], edge_types: &[&str]) {
    if ids.is_empty() {
        return;
    }
    let ids = uuid_list(ids);
    let edge_types = match edge_types
        .iter()
        .map(|value| cypher_relationship_type(value))
        .collect::<Result<Vec<_>>>()
    {
        Ok(values) => values.join("|"),
        Err(error) => {
            tracing::warn!(%error, "invalid Cortex edge type; skipping FalkorDB mirror delete");
            return;
        }
    };
    best_effort_write(pool, "delete_outgoing_edges", move |store| {
        Box::pin(async move {
            store
                .graph_query(&format!(
                    "MATCH (n:CortexNode)-[e:{edge_types}]->() WHERE n.id IN [{ids}] DELETE e"
                ))
                .await?;
            Ok(())
        })
    })
    .await;
}

pub(super) async fn mirror_demote_nodes(pool: &PgPool, ids: &[Uuid]) {
    if ids.is_empty() {
        return;
    }
    let ids = uuid_list(ids);
    best_effort_write(pool, "demote_nodes", move |store| {
        Box::pin(async move {
            store
                .graph_query(&format!(
                    "MATCH (n:CortexNode) WHERE n.id IN [{ids}] SET n.node_type = 'code:extern', n.start_line = NULL, n.end_line = NULL, n.embedding = NULL"
                ))
                .await?;
            Ok(())
        })
    })
    .await;
}

pub(crate) async fn mirror_embedding(pool: &PgPool, node_id: Uuid, embedding: &[f32]) {
    let embedding = embedding.to_vec();
    best_effort_write(pool, "store_embedding", move |store| {
        Box::pin(async move { store.store_embedding(node_id, &embedding).await })
    })
    .await;
}

/// Opt-in read router. FalkorDB is attempted only when
/// `CORTEX_GRAPH_BACKEND=falkordb`, and every failed read is transparently
/// retried against Postgres.
pub struct CortexReadRouter {
    postgres: PostgresCortexGraphStore,
    falkor: Option<FalkorCortexGraphStore>,
}

impl CortexReadRouter {
    pub async fn from_env(pool: &PgPool) -> Self {
        let postgres = PostgresCortexGraphStore::new(pool.clone());
        if std::env::var("CORTEX_GRAPH_BACKEND").as_deref() != Ok("falkordb") {
            return Self {
                postgres,
                falkor: None,
            };
        }
        let falkor = async {
            let url = std::env::var("FALKORDB_URL").context(
                "CORTEX_GRAPH_BACKEND=falkordb requires FALKORDB_URL (for example redis://priya:63379)",
            )?;
            let graph = std::env::var("FALKORDB_GRAPH").unwrap_or_else(|_| "cortex".to_string());
            let client = redis::Client::open(url).context("open FalkorDB Redis client")?;
            let connection = tokio::time::timeout(FALKOR_TIMEOUT, ConnectionManager::new(client))
                .await
                .context("FalkorDB connection timed out")?
                .context("connect to FalkorDB")?;
            Ok::<_, anyhow::Error>(FalkorCortexGraphStore::new(connection, graph, pool.clone()))
        }.await;
        match falkor {
            Ok(falkor) => Self {
                postgres,
                falkor: Some(falkor),
            },
            Err(error) => {
                tracing::warn!(%error, "FalkorDB Cortex read adapter unavailable; using Postgres");
                Self {
                    postgres,
                    falkor: None,
                }
            }
        }
    }

    async fn fallback<T, F, P>(&self, operation: &'static str, falkor: F, postgres: P) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
        P: std::future::Future<Output = Result<T>>,
    {
        if self.falkor.is_some() {
            match falkor.await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    tracing::warn!(%error, operation, "FalkorDB Cortex read failed; falling back to Postgres")
                }
            }
        }
        postgres.await
    }

    pub async fn find_symbols(
        &self,
        corpus: &str,
        query: &str,
        limit: i64,
        kind: Option<&str>,
        semantic: bool,
    ) -> Result<Vec<SymbolHit>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .find_symbols(corpus, query, limit, kind, semantic)
                .await
        };
        self.fallback(
            "find_symbols",
            falkor,
            self.postgres
                .find_symbols(corpus, query, limit, kind, semantic),
        )
        .await
    }
    pub async fn callers(
        &self,
        corpus: &str,
        symbol: &str,
        confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .callers(corpus, symbol, confidence)
                .await
        };
        self.fallback(
            "callers",
            falkor,
            self.postgres.callers(corpus, symbol, confidence),
        )
        .await
    }
    pub async fn callees(
        &self,
        corpus: &str,
        symbol: &str,
        confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .callees(corpus, symbol, confidence)
                .await
        };
        self.fallback(
            "callees",
            falkor,
            self.postgres.callees(corpus, symbol, confidence),
        )
        .await
    }
    pub async fn impact(
        &self,
        corpus: &str,
        symbol: &str,
        depth: usize,
        confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .impact(corpus, symbol, depth, confidence)
                .await
        };
        self.fallback(
            "impact",
            falkor,
            self.postgres.impact(corpus, symbol, depth, confidence),
        )
        .await
    }
    pub async fn call_path(
        &self,
        corpus: &str,
        from: &str,
        to: &str,
        depth: usize,
        confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .call_path(corpus, from, to, depth, confidence)
                .await
        };
        self.fallback(
            "call_path",
            falkor,
            self.postgres.call_path(corpus, from, to, depth, confidence),
        )
        .await
    }
    pub async fn tests_for(
        &self,
        corpus: &str,
        symbol: &str,
        depth: usize,
        confidence: f32,
    ) -> Result<Vec<TestHit>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .tests_for(corpus, symbol, depth, confidence)
                .await
        };
        self.fallback(
            "tests_for",
            falkor,
            self.postgres.tests_for(corpus, symbol, depth, confidence),
        )
        .await
    }
    pub async fn explain_community(
        &self,
        corpus: &str,
        symbol: &str,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Option<CommunityExplanation>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .explain_community(corpus, symbol, kind, limit)
                .await
        };
        self.fallback(
            "explain_community",
            falkor,
            self.postgres.explain_community(corpus, symbol, kind, limit),
        )
        .await
    }
    pub async fn outline_file(
        &self,
        corpus: &str,
        file: &str,
        kind: Option<&str>,
    ) -> Result<Option<FileOutline>> {
        let falkor = async {
            self.falkor
                .as_ref()
                .context("FalkorDB disabled")?
                .outline_file(corpus, file, kind)
                .await
        };
        self.fallback(
            "outline_file",
            falkor,
            self.postgres.outline_file(corpus, file, kind),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_graph_ro_query_rows() {
        let response = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"name".to_vec())]),
            redis::Value::Array(vec![redis::Value::Array(vec![
                redis::Value::BulkString(b"cortex::find".to_vec()),
                redis::Value::Int(7),
            ])]),
            redis::Value::Array(vec![redis::Value::BulkString(
                b"Query internal execution time: 0.1 milliseconds".to_vec(),
            )]),
        ]);
        let rows = decode_rows(response).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(value_string(&rows[0][0]).unwrap(), "cortex::find");
        assert_eq!(value_i64(&rows[0][1]).unwrap(), 7);
    }

    #[test]
    fn cypher_literals_escape_user_input() {
        assert_eq!(cypher_string("a'b\\c\n"), "'a\\'b\\\\c\\n'");
        let predicate = selector_predicate("n", "forge-fleet", "a'b");
        assert!(predicate.contains("n.project = 'forge-fleet'"));
        assert!(predicate.contains("n.title = 'a\\'b'"));
    }

    #[test]
    fn malformed_graph_response_is_rejected() {
        assert!(decode_rows(redis::Value::Int(1)).is_err());
    }

    #[test]
    fn relationship_types_cannot_inject_cypher() {
        assert_eq!(cypher_relationship_type("calls_2").unwrap(), "calls_2");
        assert!(cypher_relationship_type("calls]->() DELETE n //").is_err());
        assert!(cypher_relationship_type("2calls").is_err());
    }

    #[test]
    fn non_finite_graph_numbers_are_rejected() {
        assert!(finite_f32(f32::NAN).is_err());
        assert!(finite_f32(f32::INFINITY).is_err());
        assert_eq!(finite_f32(0.6).unwrap(), "0.6");
    }
}
