use super::{
    code_symbols::CodeSymbolsExtractor, config::ConfigExtractor, db_schema::DbSchemaExtractor,
    deps::DepsExtractor, events::EventsExtractor, lookup_code_node, observ::ObservExtractor,
    owners::OwnersExtractor, security::SecurityExtractor, upsert_code_node,
};
use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;
use std::path::PathBuf;

/// A single graph fact an extractor wants written. Confidence/provenance are mandatory.
pub enum Fact {
    Node {
        path: String,
        title: String,
        node_type: String,
        start_line: Option<i32>,
        end_line: Option<i32>,
        confidence: f32,
        provenance: String,
    },
    Edge {
        src_path: String,
        dst_path: String,
        edge_type: String,
        confidence: f32,
        provenance: String,
        method: Option<String>,
        evidence: Option<Value>,
    },
}

/// Everything an extractor needs to run against one corpus.
pub struct ExtractCtx<'a> {
    pub pool: &'a PgPool,
    pub corpus_slug: &'a str,
    pub roots: Vec<PathBuf>,
    pub generation: i64,
    pub incremental: bool,
}

#[async_trait::async_trait]
pub trait Extractor: Send + Sync {
    fn name(&self) -> &'static str;
    /// Languages/inputs this extractor handles, for logging/skip decisions.
    fn applies(&self, _ctx: &ExtractCtx) -> bool {
        true
    }
    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>>;
}

pub fn registry() -> Vec<Box<dyn Extractor>> {
    vec![
        Box::new(CodeSymbolsExtractor),
        Box::new(DbSchemaExtractor),
        Box::new(ConfigExtractor),
        Box::new(DepsExtractor),
        Box::new(EventsExtractor),
        Box::new(ObservExtractor),
        Box::new(OwnersExtractor),
        Box::new(SecurityExtractor),
    ]
}

pub async fn write_facts(
    pool: &PgPool,
    corpus_slug: &str,
    generation: i64,
    facts: &[Fact],
) -> Result<()> {
    for fact in facts {
        if let Fact::Node {
            path,
            title,
            node_type,
            start_line,
            end_line,
            confidence,
            provenance,
        } = fact
        {
            upsert_code_node(
                pool,
                path,
                title,
                node_type,
                corpus_slug,
                *start_line,
                *end_line,
                generation,
                *confidence,
                provenance,
            )
            .await?;
        }
    }

    for fact in facts {
        if let Fact::Edge {
            src_path,
            dst_path,
            edge_type,
            confidence,
            provenance,
            method,
            evidence,
        } = fact
        {
            let Some(src) = lookup_code_node(pool, src_path).await? else {
                continue;
            };
            let Some(dst) = lookup_code_node(pool, dst_path).await? else {
                continue;
            };
            super::add_edge_with_metadata(
                pool,
                src,
                dst,
                edge_type,
                *confidence,
                provenance,
                method.as_deref(),
                evidence.as_ref(),
                generation,
            )
            .await?;
        }
    }
    Ok(())
}
