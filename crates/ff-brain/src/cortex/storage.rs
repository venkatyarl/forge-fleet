//! Storage backend abstraction for Cortex.
//!
//! This is the migration scaffold for making the code graph target either the
//! current Postgres/pgvector tables or a native FalkorDB graph. The production
//! read/write path still calls the existing Postgres helpers directly; this
//! module defines the boundary the next step can route through without removing
//! the Postgres implementation.

use anyhow::Result;
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    CommunityExplanation, FileOutline, SymbolHit, SymbolRef, TestHit, add_edge_with_metadata,
    call_path, callees, callers, explain_community, find_symbols, find_symbols_semantic, impact,
    outline_file, tests_for, upsert_code_node,
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
            find_symbols_semantic(&self.pool, corpus_slug, query, limit, kind).await
        } else {
            find_symbols(&self.pool, corpus_slug, query, limit, kind).await
        }
    }

    async fn callers(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        callers(&self.pool, corpus_slug, symbol, min_confidence).await
    }

    async fn callees(
        &self,
        corpus_slug: &str,
        symbol: &str,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        callees(&self.pool, corpus_slug, symbol, min_confidence).await
    }

    async fn impact(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        impact(&self.pool, corpus_slug, symbol, max_depth, min_confidence).await
    }

    async fn call_path(
        &self,
        corpus_slug: &str,
        from: &str,
        to: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>> {
        call_path(&self.pool, corpus_slug, from, to, max_depth, min_confidence).await
    }

    async fn tests_for(
        &self,
        corpus_slug: &str,
        symbol: &str,
        max_depth: usize,
        min_confidence: f32,
    ) -> Result<Vec<TestHit>> {
        tests_for(&self.pool, corpus_slug, symbol, max_depth, min_confidence).await
    }

    async fn explain_community(
        &self,
        corpus_slug: &str,
        symbol: &str,
        kind: Option<&str>,
        member_limit: i64,
    ) -> Result<Option<CommunityExplanation>> {
        explain_community(&self.pool, corpus_slug, symbol, kind, member_limit).await
    }

    async fn outline_file(
        &self,
        corpus_slug: &str,
        file: &str,
        kind: Option<&str>,
    ) -> Result<Option<FileOutline>> {
        outline_file(&self.pool, corpus_slug, file, kind).await
    }
}

/// FalkorDB/OpenCypher implementation scaffold.
///
/// The struct owns a Redis connection manager because FalkorDB is addressed via
/// Redis commands (`GRAPH.QUERY`). The read methods are intentionally left as
/// explicit TODOs until the parser for FalkorDB's tabular responses is added;
/// the Cypher strings below document the intended query shape for each Cortex
/// operation.
#[derive(Clone)]
pub struct FalkorCortexGraphStore {
    connection: ConnectionManager,
    graph_name: String,
}

impl FalkorCortexGraphStore {
    pub fn new(connection: ConnectionManager, graph_name: impl Into<String>) -> Self {
        Self {
            connection,
            graph_name: graph_name.into(),
        }
    }

    async fn graph_query(&self, cypher: &str) -> Result<redis::Value> {
        let mut conn = self.connection.clone();
        let value = redis::cmd("GRAPH.QUERY")
            .arg(&self.graph_name)
            .arg(cypher)
            .query_async(&mut conn)
            .await?;
        Ok(value)
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
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn stable_uuid_from_path(path: &str) -> Uuid {
    let hash = Sha256::digest(path.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    Uuid::from_bytes(bytes)
}

#[async_trait]
impl CortexGraphStore for FalkorCortexGraphStore {
    fn backend_name(&self) -> &'static str {
        "falkordb"
    }

    async fn upsert_node(&self, node: &CortexGraphNode) -> Result<Uuid> {
        let id = stable_uuid_from_path(&node.path);
        let query = format!(
            "MERGE (n:CortexNode {{path: {path}}}) \
             SET n.id = {id}, n.title = {title}, n.node_type = {node_type}, \
                 n.project = {project}, n.start_line = {start_line}, \
                 n.end_line = {end_line}, n.generation = {generation}, \
                 n.confidence = {confidence}, n.provenance = {provenance}, \
                 n.valid_until = NULL \
             RETURN n.id",
            path = cypher_string(&node.path),
            id = cypher_string(&id.to_string()),
            title = cypher_string(&node.title),
            node_type = cypher_string(&node.node_type),
            project = cypher_string(&node.project),
            start_line = node
                .start_line
                .map(|n| n.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            end_line = node
                .end_line
                .map(|n| n.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            generation = node.generation,
            confidence = node.confidence,
            provenance = cypher_string(&node.provenance),
        );
        self.graph_query(&query).await?;
        Ok(id)
    }

    async fn add_edge(&self, edge: &CortexGraphEdge) -> Result<bool> {
        let query = format!(
            "MATCH (s:CortexNode {{id: {src}}}), (d:CortexNode {{id: {dst}}}) \
             MERGE (s)-[e:{edge_type}]->(d) \
             SET e.confidence = {confidence}, e.provenance = {provenance}, \
                 e.method = {method}, e.evidence = {evidence}, e.generation = {generation} \
             RETURN id(e)",
            src = cypher_string(&edge.src_id.to_string()),
            dst = cypher_string(&edge.dst_id.to_string()),
            edge_type = edge.edge_type,
            confidence = edge.confidence,
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
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
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
        _corpus_slug: &str,
        _query: &str,
        _limit: i64,
        _kind: Option<&str>,
        _semantic: bool,
    ) -> Result<Vec<SymbolHit>> {
        anyhow::bail!(
            "FalkorDB result decoding is not wired yet. Query templates:\n{FALKOR_FIND_SYMBOLS_CYPHER}\n{FALKOR_SEMANTIC_SEARCH_CYPHER}"
        )
    }

    async fn callers(
        &self,
        _corpus_slug: &str,
        _symbol: &str,
        _min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        anyhow::bail!(
            "FalkorDB result decoding is not wired yet. Query template:\n{FALKOR_CALLERS_CYPHER}"
        )
    }

    async fn callees(
        &self,
        _corpus_slug: &str,
        _symbol: &str,
        _min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        anyhow::bail!(
            "FalkorDB result decoding is not wired yet. Query template:\n{FALKOR_CALLEES_CYPHER}"
        )
    }

    async fn impact(
        &self,
        _corpus_slug: &str,
        _symbol: &str,
        _max_depth: usize,
        _min_confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        anyhow::bail!(
            "FalkorDB result decoding is not wired yet. Query template:\n{FALKOR_IMPACT_CYPHER}"
        )
    }

    async fn call_path(
        &self,
        _corpus_slug: &str,
        _from: &str,
        _to: &str,
        _max_depth: usize,
        _min_confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>> {
        anyhow::bail!(
            "FalkorDB result decoding is not wired yet. Query template:\n{FALKOR_CALL_PATH_CYPHER}"
        )
    }

    async fn tests_for(
        &self,
        _corpus_slug: &str,
        _symbol: &str,
        _max_depth: usize,
        _min_confidence: f32,
    ) -> Result<Vec<TestHit>> {
        anyhow::bail!(
            "FalkorDB tests_for should reuse the impact query then filter test-like callers"
        )
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
        _corpus_slug: &str,
        _file: &str,
        _kind: Option<&str>,
    ) -> Result<Option<FileOutline>> {
        anyhow::bail!(
            "FalkorDB result decoding is not wired yet. Query template:\n{FALKOR_OUTLINE_FILE_CYPHER}"
        )
    }
}
