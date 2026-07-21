//! Authoritative FalkorDB storage backend for Cortex.
//!
//! This module is intentionally not wired into Cortex yet.  Unlike the
//! migration adapter in [`super::storage`], writes made here are FalkorDB-only
//! and are rejected while either Postgres graph table contains data.

use anyhow::{Context, Result};
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

use super::storage::{CortexGraphEdge, CortexGraphNode, CortexGraphStore, FalkorCortexGraphStore};
use super::{CommunityExplanation, FileOutline, SymbolHit, SymbolRef, TestHit};

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Small FalkorDB client used by the authoritative backend.
#[derive(Clone)]
pub struct FalkorDBClient {
    connection: ConnectionManager,
    graph_name: String,
}

impl FalkorDBClient {
    pub fn new(connection: ConnectionManager, graph_name: impl Into<String>) -> Self {
        Self {
            connection,
            graph_name: graph_name.into(),
        }
    }

    async fn query(&self, cypher: &str) -> Result<redis::Value> {
        let mut connection = self.connection.clone();
        tokio::time::timeout(
            QUERY_TIMEOUT,
            redis::cmd("GRAPH.QUERY")
                .arg(&self.graph_name)
                .arg(cypher)
                .arg("TIMEOUT")
                .arg(QUERY_TIMEOUT.as_millis() as u64)
                .query_async(&mut connection),
        )
        .await
        .context("FalkorDB write timed out")?
        .context("FalkorDB write failed")
    }
}

/// FalkorDB-authoritative Cortex graph backend, ready for integration.
///
/// Nodes carry `CortexNode` plus labels derived from their colon-delimited
/// type (for example `code:function` becomes `:Code:Function`). Edges use their
/// graph type as the relationship type and retain both valid-time and
/// transaction-time history.
#[derive(Clone)]
pub struct FalkorDBBackend {
    client: FalkorDBClient,
    postgres: PgPool,
    reads: FalkorCortexGraphStore,
}

impl FalkorDBBackend {
    pub fn new(client: FalkorDBClient, postgres: PgPool) -> Self {
        let reads = FalkorCortexGraphStore::new(
            client.connection.clone(),
            client.graph_name.clone(),
            postgres.clone(),
        );
        Self {
            client,
            postgres,
            reads,
        }
    }

    /// Prevent split-brain writes while the legacy Postgres graph has data.
    async fn ensure_postgres_graph_empty(&self) -> Result<()> {
        let empty: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM brain_vault_nodes LIMIT 1) \
                    AND NOT EXISTS (SELECT 1 FROM brain_vault_edges LIMIT 1)",
        )
        .fetch_one(&self.postgres)
        .await
        .context("checking that Postgres graph tables are empty")?;
        if !empty {
            anyhow::bail!(
                "refusing authoritative FalkorDB write: Postgres graph tables are not empty"
            );
        }
        Ok(())
    }

    fn node_query(id: Uuid, node: &CortexGraphNode) -> Result<String> {
        let labels = typed_labels(&node.node_type)?;
        Ok(format!(
            "MERGE (n:CortexNode {{project: {project}, path: {path}}}) \
             SET n{labels}, n.id={id}, n.title={title}, n.node_type={node_type}, \
                 n.project={project}, n.start_line={start_line}, n.end_line={end_line}, \
                 n.generation={generation}, n.confidence={confidence}, \
                 n.provenance={provenance}, n.valid_from=coalesce(n.valid_from, timestamp()), \
                 n.valid_until=NULL RETURN n.id",
            path = cypher_string(&node.path),
            id = cypher_string(&id.to_string()),
            title = cypher_string(&node.title),
            node_type = cypher_string(&node.node_type),
            project = cypher_string(&node.project),
            start_line = optional_i32(node.start_line),
            end_line = optional_i32(node.end_line),
            generation = node.generation,
            confidence = finite_f32(node.confidence)?,
            provenance = cypher_string(&node.provenance),
        ))
    }

    fn edge_query(edge: &CortexGraphEdge) -> Result<String> {
        let edge_type = graph_identifier(&edge.edge_type)?;
        Ok(format!(
            "MATCH (s:CortexNode {{id: {src}}}), (d:CortexNode {{id: {dst}}}) \
             OPTIONAL MATCH (s)-[old:{edge_type}]->(d) \
             WHERE old.valid_until IS NULL AND old.tx_until IS NULL \
               AND old.generation <> {generation} \
             FOREACH (r IN CASE WHEN old IS NULL THEN [] ELSE [old] END | \
                 SET r.valid_until=timestamp(), r.tx_until=timestamp()) \
             MERGE (s)-[e:{edge_type} {{generation: {generation}}}]->(d) \
             SET e.confidence={confidence}, e.provenance={provenance}, \
                 e.method={method}, e.evidence={evidence}, \
                 e.valid_from=coalesce(e.valid_from, timestamp()), e.valid_until=NULL, \
                 e.tx_from=coalesce(e.tx_from, timestamp()), e.tx_until=NULL \
             RETURN id(e)",
            src = cypher_string(&edge.src_id.to_string()),
            dst = cypher_string(&edge.dst_id.to_string()),
            confidence = finite_f32(edge.confidence)?,
            provenance = cypher_string(&edge.provenance),
            method = edge
                .method
                .as_deref()
                .map(cypher_string)
                .unwrap_or_else(|| "NULL".into()),
            evidence = edge
                .evidence
                .as_ref()
                .map(|v| cypher_string(&v.to_string()))
                .unwrap_or_else(|| "NULL".into()),
            generation = edge.generation,
        ))
    }
}

#[async_trait]
impl CortexGraphStore for FalkorDBBackend {
    fn backend_name(&self) -> &'static str {
        "falkordb-authoritative"
    }

    async fn upsert_node(&self, node: &CortexGraphNode) -> Result<Uuid> {
        self.ensure_postgres_graph_empty().await?;
        let id = stable_uuid(&node.project, &node.path);
        self.client.query(&Self::node_query(id, node)?).await?;
        Ok(id)
    }

    async fn add_edge(&self, edge: &CortexGraphEdge) -> Result<bool> {
        self.ensure_postgres_graph_empty().await?;
        self.client.query(&Self::edge_query(edge)?).await?;
        Ok(true)
    }

    async fn wipe_code_nodes(&self, corpus_slug: &str) -> Result<()> {
        self.ensure_postgres_graph_empty().await?;
        self.client.query(&format!(
            "MATCH (n:CortexNode) WHERE n.project={} AND n.node_type STARTS WITH 'code:' DETACH DELETE n",
            cypher_string(corpus_slug)
        )).await?;
        Ok(())
    }

    async fn store_embedding(&self, node_id: Uuid, embedding: &[f32]) -> Result<()> {
        self.ensure_postgres_graph_empty().await?;
        let values = embedding
            .iter()
            .map(|v| finite_f32(*v))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        self.client
            .query(&format!(
                "MATCH (n:CortexNode {{id: {}}}) SET n.embedding=vecf32([{}]) RETURN n.id",
                cypher_string(&node_id.to_string()),
                values
            ))
            .await?;
        Ok(())
    }

    async fn find_symbols(
        &self,
        corpus: &str,
        query: &str,
        limit: i64,
        kind: Option<&str>,
        semantic: bool,
    ) -> Result<Vec<SymbolHit>> {
        self.reads
            .find_symbols(corpus, query, limit, kind, semantic)
            .await
    }
    async fn callers(&self, corpus: &str, symbol: &str, confidence: f32) -> Result<Vec<SymbolRef>> {
        self.reads.callers(corpus, symbol, confidence).await
    }
    async fn callees(&self, corpus: &str, symbol: &str, confidence: f32) -> Result<Vec<SymbolRef>> {
        self.reads.callees(corpus, symbol, confidence).await
    }
    async fn impact(
        &self,
        corpus: &str,
        symbol: &str,
        depth: usize,
        confidence: f32,
    ) -> Result<Vec<SymbolRef>> {
        self.reads.impact(corpus, symbol, depth, confidence).await
    }
    async fn call_path(
        &self,
        corpus: &str,
        from: &str,
        to: &str,
        depth: usize,
        confidence: f32,
    ) -> Result<Option<Vec<SymbolRef>>> {
        self.reads
            .call_path(corpus, from, to, depth, confidence)
            .await
    }
    async fn tests_for(
        &self,
        corpus: &str,
        symbol: &str,
        depth: usize,
        confidence: f32,
    ) -> Result<Vec<TestHit>> {
        self.reads
            .tests_for(corpus, symbol, depth, confidence)
            .await
    }
    async fn explain_community(
        &self,
        corpus: &str,
        symbol: &str,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Option<CommunityExplanation>> {
        self.reads
            .explain_community(corpus, symbol, kind, limit)
            .await
    }
    async fn outline_file(
        &self,
        corpus: &str,
        file: &str,
        kind: Option<&str>,
    ) -> Result<Option<FileOutline>> {
        self.reads.outline_file(corpus, file, kind).await
    }
}

fn typed_labels(node_type: &str) -> Result<String> {
    node_type
        .split(':')
        .map(|part| {
            graph_identifier(part).map(|part| {
                let mut chars = part.chars();
                format!(
                    ":{}{}",
                    chars.next().unwrap().to_ascii_uppercase(),
                    chars.as_str()
                )
            })
        })
        .collect()
}

fn graph_identifier(value: &str) -> Result<&str> {
    if !value.is_empty()
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        Ok(value)
    } else {
        anyhow::bail!("invalid FalkorDB graph identifier {value:?}")
    }
}

fn cypher_string(value: &str) -> String {
    format!(
        "'{}'",
        value
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn optional_i32(value: Option<i32>) -> String {
    value.map_or_else(|| "NULL".into(), |v| v.to_string())
}

fn finite_f32(value: f32) -> Result<String> {
    if value.is_finite() {
        Ok(value.to_string())
    } else {
        anyhow::bail!("non-finite float cannot be written to FalkorDB")
    }
}

fn stable_uuid(project: &str, path: &str) -> Uuid {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{project}\0{path}").as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 variant and a name-based version marker.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_types_become_typed_labels() {
        assert_eq!(typed_labels("code:function").unwrap(), ":Code:Function");
        assert!(typed_labels("code:bad-label").is_err());
    }

    #[test]
    fn node_identity_is_scoped_to_project() {
        assert_ne!(
            stable_uuid("one", "code://item"),
            stable_uuid("two", "code://item")
        );
        assert_eq!(stable_uuid("one", "code://item").get_version_num(), 5);
    }

    #[test]
    fn edge_query_is_bitemporal() {
        let edge = CortexGraphEdge {
            src_id: Uuid::nil(),
            dst_id: Uuid::max(),
            edge_type: "calls".into(),
            generation: 7,
            confidence: 0.9,
            provenance: "test".into(),
            method: None,
            evidence: None,
        };
        let query = FalkorDBBackend::edge_query(&edge).unwrap();
        for field in ["valid_from", "valid_until", "tx_from", "tx_until"] {
            assert!(query.contains(field));
        }
    }
}
