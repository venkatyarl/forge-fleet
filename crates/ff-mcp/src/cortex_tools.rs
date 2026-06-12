//! Cortex code-graph MCP tools — let fleet agents consume the pre-indexed code
//! graph the way they consume the CRG MCP server (the "CodeGraph" pattern:
//! a local graph served over MCP, fewer tokens / tool-calls than file scanning).
//!
//! All read-only: `cortex_corpora` (discover indexed repos), `cortex_callers`,
//! `cortex_callees`, and `cortex_impact` (transitive blast radius). The graph is
//! built by `ff cortex index`; these tools only query it.

use ff_brain::{callees, callers, corpus, impact};
use ff_core::config;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;

use crate::handlers::HandlerResult;

/// Get a Postgres pool using the fleet config (same pattern as brain_tools).
async fn get_pool() -> Result<sqlx::PgPool, String> {
    let (cfg, _) =
        config::load_config_auto().map_err(|e| format!("failed to load fleet config: {e}"))?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&cfg.database.url)
        .await
        .map_err(|e| format!("Postgres connection failed: {e}"))
}

/// Pull the required `corpus` slug + `symbol` selector out of the params.
fn corpus_and_symbol(params: &Option<Value>) -> Result<(String, String), String> {
    let corpus = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?;
    let symbol = params
        .as_ref()
        .and_then(|p| p.get("symbol"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: symbol".to_string())?;
    Ok((corpus.to_string(), symbol.to_string()))
}

/// Shape a `Vec<SymbolRef>` into the JSON result list.
fn symbols_json(symbols: &[ff_brain::SymbolRef]) -> Vec<Value> {
    symbols
        .iter()
        .map(|s| {
            json!({
                "qualified_name": s.qualified_name,
                "node_type": s.node_type,
                "id": s.id.to_string(),
            })
        })
        .collect()
}

/// List the indexed Cortex corpora (repos) so an agent can discover valid slugs.
pub async fn cortex_corpora(_params: Option<Value>) -> HandlerResult {
    let pool = get_pool().await?;
    let corpora = corpus::list_corpora(&pool)
        .await
        .map_err(|e| format!("list corpora: {e}"))?;
    Ok(json!({
        "count": corpora.len(),
        "corpora": corpora.iter().map(|c| json!({
            "slug": c.slug,
            "title": c.title,
            "sources": c.sources,
            "content_nodes": c.content,
        })).collect::<Vec<_>>()
    }))
}

/// Callers of a code symbol (who calls it).
pub async fn cortex_callers(params: Option<Value>) -> HandlerResult {
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let pool = get_pool().await?;
    let rows = callers(&pool, &corpus_slug, &symbol)
        .await
        .map_err(|e| format!("callers: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "count": rows.len(),
        "callers": symbols_json(&rows),
    }))
}

/// Callees of a code symbol (what it calls).
pub async fn cortex_callees(params: Option<Value>) -> HandlerResult {
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let pool = get_pool().await?;
    let rows = callees(&pool, &corpus_slug, &symbol)
        .await
        .map_err(|e| format!("callees: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "count": rows.len(),
        "callees": symbols_json(&rows),
    }))
}

/// Transitive caller closure / blast radius of a code symbol.
pub async fn cortex_impact(params: Option<Value>) -> HandlerResult {
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let max_depth = params
        .as_ref()
        .and_then(|p| p.get("max_depth"))
        .and_then(|v| v.as_u64())
        .map(|d| d.clamp(1, 20) as usize)
        .unwrap_or(5);
    let pool = get_pool().await?;
    let rows = impact(&pool, &corpus_slug, &symbol, max_depth)
        .await
        .map_err(|e| format!("impact: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "max_depth": max_depth,
        "count": rows.len(),
        "impacted": symbols_json(&rows),
    }))
}
