//! Cortex code-graph MCP tools — let fleet agents consume the pre-indexed code
//! graph the way they consume the CRG MCP server (the "CodeGraph" pattern:
//! a local graph served over MCP, fewer tokens / tool-calls than file scanning).
//!
//! All read-only: `cortex_corpora` (discover indexed repos), `cortex_callers`,
//! `cortex_callees`, `cortex_impact` (transitive blast radius), and `cortex_path`
//! (shortest call chain between two symbols). The graph is built by
//! `ff cortex index`; these tools only query it.

use ff_brain::{
    call_path, callees, callees_all_corpora, callers, callers_all_corpora, corpus, cortex,
    find_symbols, find_symbols_all_corpora, find_symbols_semantic, impact, impact_all_corpora,
    tests_for,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::handlers::HandlerResult;

/// Get the process-shared Postgres pool (cached once — never per call; see
/// [`crate::pool`]).
async fn get_pool() -> Result<sqlx::PgPool, String> {
    crate::pool::shared_pg_pool().await
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

/// Optional `corpus` for tools that can sensibly default to the cwd slug.
fn corpus_or_cwd_slug(params: &Option<Value>) -> Result<String, String> {
    if let Some(corpus) = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
    {
        return Ok(corpus.to_string());
    }

    std::env::current_dir()
        .map_err(|e| format!("default corpus from cwd: {e}"))?
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing corpus and could not derive default from cwd".to_string())
}

fn symbol_param(params: &Option<Value>) -> Result<String, String> {
    params
        .as_ref()
        .and_then(|p| p.get("symbol"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing required parameter: symbol".to_string())
}

fn bool_param(params: &Option<Value>, name: &str, default: bool) -> bool {
    params
        .as_ref()
        .and_then(|p| p.get(name))
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

fn capped_usize_param(params: &Option<Value>, name: &str, default: usize, max: usize) -> usize {
    params
        .as_ref()
        .and_then(|p| p.get(name))
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(1, max))
        .unwrap_or(default)
}

/// Shape a `Vec<SymbolRef>` into the JSON result list.
fn symbols_json(symbols: &[ff_brain::SymbolRef]) -> Vec<Value> {
    symbols
        .iter()
        .map(|s| {
            json!({
                "qualified_name": s.qualified_name,
                "node_type": s.node_type,
                "file": s.file,
                "start_line": s.start_line,
                "id": s.id.to_string(),
            })
        })
        .collect()
}

fn capped_symbols_json(symbols: &[ff_brain::SymbolRef], limit: usize) -> Vec<Value> {
    symbols_json(&symbols[..symbols.len().min(limit)])
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

/// Find code symbols by name fragment — the discovery entrypoint. An agent that
/// knows part of a name gets the exact qualified names (ranked by fan-in) to
/// then feed into cortex_callers/callees/impact, instead of grepping for them.
pub async fn cortex_find(params: Option<Value>) -> HandlerResult {
    let corpus_slug = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?
        .to_string();
    let query = params
        .as_ref()
        .and_then(|p| p.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: query".to_string())?
        .to_string();
    let limit = params
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(20);
    let semantic = params
        .as_ref()
        .and_then(|p| p.get("semantic"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let kind = params
        .as_ref()
        .and_then(|p| p.get("kind"))
        .and_then(|v| v.as_str());
    let pool = get_pool().await?;
    let hits = if semantic {
        find_symbols_semantic(&pool, &corpus_slug, &query, limit, kind)
            .await
            .map_err(|e| format!("find (semantic): {e}"))?
    } else {
        find_symbols(&pool, &corpus_slug, &query, limit, kind)
            .await
            .map_err(|e| format!("find: {e}"))?
    };
    Ok(json!({
        "corpus": corpus_slug,
        "query": query,
        "semantic": semantic,
        "count": hits.len(),
        "hits": hits.iter().map(|h| json!({
            "qualified_name": h.qualified_name,
            "node_type": h.node_type,
            "file": h.file,
            "start_line": h.start_line,
            "fan_in": h.fan_in,
            "score": h.score,
            "id": h.id.to_string(),
        })).collect::<Vec<_>>(),
    }))
}

/// Hybrid code search for natural-language intent: semantic vector search,
/// graph-neighborhood expansion, then cross-encoder rerank.
pub async fn cortex_search(params: Option<Value>) -> HandlerResult {
    let corpus_slug = corpus_or_cwd_slug(&params)?;
    let query = params
        .as_ref()
        .and_then(|p| p.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: query".to_string())?
        .to_string();
    let limit = capped_usize_param(&params, "limit", 8, 50);
    let pool = get_pool().await?;
    let hits = ff_brain::cortex_search(&pool, &corpus_slug, &query, limit)
        .await
        .map_err(|e| format!("cortex_search: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "query": query,
        "count": hits.len(),
        "hits": hits.iter().map(|h| json!({
            "qualified_name": h.qualified_name,
            "node_type": h.node_type,
            "file": h.file,
            "start_line": h.start_line,
            "fan_in": h.fan_in,
            "score": h.score,
            "id": h.id.to_string(),
        })).collect::<Vec<_>>(),
    }))
}

/// Cross-repo symbol search: find a name across EVERY indexed corpus at once
/// (monorepo / multi-repo navigation), each hit tagged with its repo. The
/// multi-corpus counterpart of `cortex_find` — answers "where does `foo` live
/// across all my repos?" without first picking a corpus.
pub async fn cortex_cross_repo_find(params: Option<Value>) -> HandlerResult {
    let query = params
        .as_ref()
        .and_then(|p| p.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: query".to_string())?
        .to_string();
    let limit = params
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(20);
    let kind = params
        .as_ref()
        .and_then(|p| p.get("kind"))
        .and_then(|v| v.as_str());
    let pool = get_pool().await?;
    let hits = find_symbols_all_corpora(&pool, &query, limit, kind)
        .await
        .map_err(|e| format!("cross_repo_find: {e}"))?;
    Ok(json!({
        "query": query,
        "count": hits.len(),
        "hits": hits.iter().map(|(corpus, h)| json!({
            "corpus": corpus,
            "qualified_name": h.qualified_name,
            "node_type": h.node_type,
            "file": h.file,
            "start_line": h.start_line,
            "fan_in": h.fan_in,
            "id": h.id.to_string(),
        })).collect::<Vec<_>>(),
    }))
}

/// Show a code symbol's source — resolve a name to its file + line span and
/// return just that symbol's definition. The Cortex-native `get_review_context`:
/// one call instead of cortex_find → read the file → slice the span.
pub async fn cortex_show(params: Option<Value>) -> HandlerResult {
    let corpus_slug = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?
        .to_string();
    let symbol = params
        .as_ref()
        .and_then(|p| p.get("symbol"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: symbol".to_string())?
        .to_string();
    let kind = params
        .as_ref()
        .and_then(|p| p.get("kind"))
        .and_then(|v| v.as_str());
    let max_lines = params
        .as_ref()
        .and_then(|p| p.get("max_lines"))
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(1, 2000) as usize)
        .unwrap_or(200);
    let context = params
        .as_ref()
        .and_then(|p| p.get("context"))
        .and_then(|v| v.as_u64())
        .map(|n| n.min(500) as usize)
        .unwrap_or(0);
    let pool = get_pool().await?;
    let found = ff_brain::show_symbol(&pool, &corpus_slug, &symbol, kind, max_lines, context)
        .await
        .map_err(|e| format!("show: {e}"))?;
    match found {
        None => Ok(json!({
            "corpus": corpus_slug,
            "symbol": symbol,
            "found": false,
        })),
        Some(s) => Ok(json!({
            "corpus": corpus_slug,
            "found": true,
            "qualified_name": s.qualified_name,
            "node_type": s.node_type,
            "file": s.file,
            "start_line": s.start_line,
            "end_line": s.end_line,
            "display_start": s.display_start,
            "fan_in": s.fan_in,
            "truncated": s.truncated,
            "source": s.source,
            "other_matches": s.other_matches,
        })),
    }
}

/// One Cortex call for the standard agent orientation loop: resolve a symbol,
/// return its definition source, direct callers/callees, blast-radius count, and
/// the symbol's community summary. This composes the existing Cortex query
/// helpers so the graph semantics stay centralized in `ff_brain::cortex`.
pub async fn cortex_context(params: Option<Value>) -> HandlerResult {
    let corpus_slug = corpus_or_cwd_slug(&params)?;
    let symbol = symbol_param(&params)?;
    let include_snippet = bool_param(&params, "include_snippet", true);
    let max_callers = capped_usize_param(&params, "max_callers", 10, 100);
    let max_callees = capped_usize_param(&params, "max_callees", 10, 100);
    let min_confidence = min_confidence_param(&params);
    let impact_depth = 5;

    let pool = get_pool().await?;
    let shown = ff_brain::show_symbol(&pool, &corpus_slug, &symbol, None, 200, 0)
        .await
        .map_err(|e| format!("context/show: {e}"))?;
    let Some(s) = shown else {
        return Ok(json!({
            "corpus": corpus_slug,
            "symbol": symbol,
            "found": false,
        }));
    };

    let resolved_symbol = s.qualified_name.clone();
    let caller_rows = callers(&pool, &corpus_slug, &resolved_symbol, min_confidence)
        .await
        .map_err(|e| format!("context/callers: {e}"))?;
    let callee_rows = callees(&pool, &corpus_slug, &resolved_symbol, min_confidence)
        .await
        .map_err(|e| format!("context/callees: {e}"))?;
    let impacted = impact(
        &pool,
        &corpus_slug,
        &resolved_symbol,
        impact_depth,
        min_confidence,
    )
    .await
    .map_err(|e| format!("context/impact: {e}"))?;
    let community = ff_brain::explain_community(&pool, &corpus_slug, &resolved_symbol, None, 1)
        .await
        .map_err(|e| format!("context/community: {e}"))?;

    let snippet = if include_snippet {
        Some(json!({
            "display_start": s.display_start,
            "source": s.source,
            "truncated": s.truncated,
        }))
    } else {
        None
    };

    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "found": true,
        "resolved": {
            "name": resolved_symbol,
            "path": format!("code://{}/{}", corpus_slug, s.qualified_name),
            "kind": s.node_type,
            "file": s.file,
            "span": {
                "start_line": s.start_line,
                "end_line": s.end_line,
            },
        },
        "snippet": snippet,
        "callers": capped_symbols_json(&caller_rows, max_callers),
        "callees": capped_symbols_json(&callee_rows, max_callees),
        "impact": {
            "count": impacted.len(),
            "max_depth": impact_depth,
            "min_confidence": min_confidence,
        },
        "community": community.map(|c| json!({
            "id": c.community_id,
            "summary": c.summary,
        })),
        "disambiguation": s.other_matches,
    }))
}

/// Explain the subsystem a symbol belongs to — resolve it to its code-graph
/// community and return that community's natural-language summary (from
/// `ff cortex summarize`) plus its highest-fan-in members. The GraphRAG
/// "what is this cluster responsible for?" answer in one call, so an agent can
/// orient on a subsystem without reading every file in it.
pub async fn cortex_explain(params: Option<Value>) -> HandlerResult {
    let corpus_slug = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?
        .to_string();
    let symbol = params
        .as_ref()
        .and_then(|p| p.get("symbol"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: symbol".to_string())?
        .to_string();
    let kind = params
        .as_ref()
        .and_then(|p| p.get("kind"))
        .and_then(|v| v.as_str());
    let members = params
        .as_ref()
        .and_then(|p| p.get("members"))
        .and_then(|v| v.as_i64())
        .unwrap_or(15);
    let pool = get_pool().await?;
    let found = ff_brain::explain_community(&pool, &corpus_slug, &symbol, kind, members)
        .await
        .map_err(|e| format!("explain: {e}"))?;
    match found {
        None => Ok(json!({
            "corpus": corpus_slug,
            "symbol": symbol,
            "found": false,
        })),
        Some(e) => Ok(json!({
            "corpus": corpus_slug,
            "found": true,
            "resolved_symbol": e.resolved_symbol,
            "resolved_node_type": e.resolved_node_type,
            "community_id": e.community_id,
            "member_count": e.member_count,
            "summary": e.summary,
            "summary_model": e.summary_model,
            "god_symbol": e.god_symbol,
            "members": e.members.iter().map(|m| json!({
                "symbol": m.qualified_name,
                "node_type": m.node_type,
                "fan_in": m.fan_in,
            })).collect::<Vec<_>>(),
            // GraphRAG subsystem hierarchy above this community (immediate parent
            // first): "what larger subsystem is this part of, at each scope?".
            "subsystem_chain": e.subsystem_chain.iter().map(|s| json!({
                "level": s.level,
                "member_count": s.member_count,
                "summary": s.summary,
                "god_symbol": s.god_symbol,
            })).collect::<Vec<_>>(),
        })),
    }
}

/// Outline a file — every code symbol it defines (kind / line span / fan-in) in
/// source order. A file-level table of contents so an agent can orient in an
/// unknown file from the graph instead of reading the whole file.
pub async fn cortex_outline(params: Option<Value>) -> HandlerResult {
    let corpus_slug = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?
        .to_string();
    let file = params
        .as_ref()
        .and_then(|p| p.get("file"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: file".to_string())?
        .to_string();
    let kind = params
        .as_ref()
        .and_then(|p| p.get("kind"))
        .and_then(|v| v.as_str());
    let pool = get_pool().await?;
    let found = ff_brain::outline_file(&pool, &corpus_slug, &file, kind)
        .await
        .map_err(|e| format!("outline: {e}"))?;
    match found {
        None => Ok(json!({
            "corpus": corpus_slug,
            "file": file,
            "found": false,
        })),
        Some(o) => Ok(json!({
            "corpus": corpus_slug,
            "found": true,
            "file": o.file,
            "count": o.symbols.len(),
            "symbols": o.symbols.iter().map(|s| json!({
                "qualified_name": s.qualified_name,
                "node_type": s.node_type,
                "start_line": s.start_line,
                "end_line": s.end_line,
                "fan_in": s.fan_in,
            })).collect::<Vec<_>>(),
        })),
    }
}

/// Extract the optional `min_confidence` edge-tier filter (roadmap #5): clamps to
/// [0.0, 1.0], default 0.0 (traverse every `calls` edge). 1.0 = EXTRACTED only
/// (high-trust), 0.6 = +INFERRED.
fn min_confidence_param(params: &Option<Value>) -> f32 {
    params
        .as_ref()
        .and_then(|p| p.get("min_confidence"))
        .and_then(|v| v.as_f64())
        .map(|c| c.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0)
}

/// Callers of a code symbol (who calls it).
pub async fn cortex_callers(params: Option<Value>) -> HandlerResult {
    let min_confidence = min_confidence_param(&params);
    let pool = get_pool().await?;
    if bool_param(&params, "all_corpora", false) {
        let symbol = symbol_param(&params)?;
        let hits = callers_all_corpora(&pool, &symbol, min_confidence)
            .await
            .map_err(|e| format!("callers: {e}"))?;
        return Ok(json!({
            "symbol": symbol,
            "all_corpora": true,
            "min_confidence": min_confidence,
            "count": hits.len(),
            "callers": cross_corpus_symbols_json(&hits),
        }));
    }
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let rows = callers(&pool, &corpus_slug, &symbol, min_confidence)
        .await
        .map_err(|e| format!("callers: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "min_confidence": min_confidence,
        "count": rows.len(),
        "callers": symbols_json(&rows),
    }))
}

/// Serialize cross-corpus callers/callees, each tagged with its corpus.
fn cross_corpus_symbols_json(hits: &[(String, ff_brain::SymbolRef)]) -> Vec<Value> {
    hits.iter()
        .map(|(corpus, s)| {
            json!({
                "corpus": corpus,
                "qualified_name": s.qualified_name,
                "node_type": s.node_type,
                "file": s.file,
                "start_line": s.start_line,
                "id": s.id.to_string(),
            })
        })
        .collect()
}

/// Callees of a code symbol (what it calls).
pub async fn cortex_callees(params: Option<Value>) -> HandlerResult {
    let min_confidence = min_confidence_param(&params);
    let pool = get_pool().await?;
    if bool_param(&params, "all_corpora", false) {
        let symbol = symbol_param(&params)?;
        let hits = callees_all_corpora(&pool, &symbol, min_confidence)
            .await
            .map_err(|e| format!("callees: {e}"))?;
        return Ok(json!({
            "symbol": symbol,
            "all_corpora": true,
            "min_confidence": min_confidence,
            "count": hits.len(),
            "callees": cross_corpus_symbols_json(&hits),
        }));
    }
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let rows = callees(&pool, &corpus_slug, &symbol, min_confidence)
        .await
        .map_err(|e| format!("callees: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "min_confidence": min_confidence,
        "count": rows.len(),
        "callees": symbols_json(&rows),
    }))
}

/// Transitive caller closure / blast radius of a code symbol.
pub async fn cortex_impact(params: Option<Value>) -> HandlerResult {
    let min_confidence = min_confidence_param(&params);
    let max_depth = params
        .as_ref()
        .and_then(|p| p.get("max_depth"))
        .and_then(|v| v.as_u64())
        .map(|d| d.clamp(1, 20) as usize)
        .unwrap_or(5);
    let pool = get_pool().await?;
    if bool_param(&params, "all_corpora", false) {
        let symbol = symbol_param(&params)?;
        let hits = impact_all_corpora(&pool, &symbol, max_depth, min_confidence)
            .await
            .map_err(|e| format!("impact: {e}"))?;
        return Ok(json!({
            "symbol": symbol,
            "all_corpora": true,
            "max_depth": max_depth,
            "min_confidence": min_confidence,
            "count": hits.len(),
            "impacted": cross_corpus_symbols_json(&hits),
        }));
    }
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let rows = impact(&pool, &corpus_slug, &symbol, max_depth, min_confidence)
        .await
        .map_err(|e| format!("impact: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "max_depth": max_depth,
        "min_confidence": min_confidence,
        "count": rows.len(),
        "impacted": symbols_json(&rows),
    }))
}

/// Dependency graph. With no `crate`: list dependency packages and how many
/// crates depend on each. With a `crate`: that crate's forward dependencies
/// (what it needs) + reverse dependents (what needs it — the rebuild blast
/// radius); `transitive=true` adds the full transitive-dependents closure.
pub async fn cortex_deps(params: Option<Value>) -> HandlerResult {
    let pool = get_pool().await?;
    let corpus_slug = corpus_or_cwd_slug(&params)?;
    let crate_name = params
        .as_ref()
        .and_then(|p| p.get("crate"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    match crate_name {
        Some(name) => {
            let mut cd = cortex::deps::deps_for_crate(&pool, name, Some(&corpus_slug))
                .await
                .map_err(|e| format!("deps: {e}"))?;
            if bool_param(&params, "transitive", false) {
                cd.transitive_dependents = Some(
                    cortex::deps::transitive_dependents(&pool, name, Some(&corpus_slug))
                        .await
                        .map_err(|e| format!("transitive_dependents: {e}"))?,
                );
            }
            let deps_json = serde_json::to_value(&cd).map_err(|e| format!("serialize: {e}"))?;
            Ok(json!({ "corpus": corpus_slug, "crate": name, "deps": deps_json }))
        }
        None => {
            let rows = cortex::deps::deps(&pool, Some(&corpus_slug))
                .await
                .map_err(|e| format!("deps: {e}"))?;
            let packages = serde_json::to_value(&rows).map_err(|e| format!("serialize: {e}"))?;
            Ok(json!({ "corpus": corpus_slug, "count": rows.len(), "packages": packages }))
        }
    }
}

/// Functions that READ a database column (the column's data-flow inbound side).
/// Use before changing a column's type/meaning to see who consumes it.
pub async fn cortex_readers(params: Option<Value>) -> HandlerResult {
    cortex_column_accessors(params, true).await
}

/// Functions that WRITE a database column (who produces its value). Use before
/// changing a column's invariants to see every write site.
pub async fn cortex_writers(params: Option<Value>) -> HandlerResult {
    cortex_column_accessors(params, false).await
}

async fn cortex_column_accessors(params: Option<Value>, reads: bool) -> HandlerResult {
    let pool = get_pool().await?;
    let corpus_slug = corpus_or_cwd_slug(&params)?;
    let column = params
        .as_ref()
        .and_then(|p| p.get("column"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "missing required parameter: column (a DB column, e.g. 'work_items.status' or 'status')"
                .to_string()
        })?;
    let rows = if reads {
        cortex::dataflow::readers(&pool, Some(&corpus_slug), column).await
    } else {
        cortex::dataflow::writers(&pool, Some(&corpus_slug), column).await
    }
    .map_err(|e| format!("{}: {e}", if reads { "readers" } else { "writers" }))?;
    let accessors = serde_json::to_value(&rows).map_err(|e| format!("serialize: {e}"))?;
    Ok(json!({
        "corpus": corpus_slug,
        "column": column,
        "direction": if reads { "reads" } else { "writes" },
        "count": rows.len(),
        "accessors": accessors,
    }))
}

/// Shortest call chain from one symbol to another (HOW does `from` reach `to`).
/// `callers`/`callees` answer one hop and `impact` the whole closure; this returns
/// the ordered FROM → … → TO path (each hop a real `calls` edge). An empty `path`
/// with `found=false` means the two symbols exist but don't connect within
/// `max_depth` (legitimate, not an error); an unresolved from/to symbol errors.
pub async fn cortex_path(params: Option<Value>) -> HandlerResult {
    let corpus_slug = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?;
    let from = params
        .as_ref()
        .and_then(|p| p.get("from"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: from (the start symbol)".to_string())?;
    let to = params
        .as_ref()
        .and_then(|p| p.get("to"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: to (the target symbol)".to_string())?;
    let max_depth = params
        .as_ref()
        .and_then(|p| p.get("max_depth"))
        .and_then(|v| v.as_u64())
        .map(|d| d.clamp(1, 30) as usize)
        .unwrap_or(12);
    let min_confidence = min_confidence_param(&params);
    let pool = get_pool().await?;
    let path = call_path(&pool, corpus_slug, from, to, max_depth, min_confidence)
        .await
        .map_err(|e| format!("path: {e}"))?;
    let rows = path.unwrap_or_default();
    Ok(json!({
        "corpus": corpus_slug,
        "from": from,
        "to": to,
        "max_depth": max_depth,
        "min_confidence": min_confidence,
        "found": !rows.is_empty(),
        // hops = edges traversed (one less than nodes); 0 = from and to are the same node
        "hops": rows.len().saturating_sub(1),
        "path": symbols_json(&rows),
    }))
}

/// Tests covering a code symbol: the transitive caller closure filtered to the
/// callers that are tests, ranked nearest-first. Empty = coverage gap.
pub async fn cortex_tests(params: Option<Value>) -> HandlerResult {
    let (corpus_slug, symbol) = corpus_and_symbol(&params)?;
    let max_depth = params
        .as_ref()
        .and_then(|p| p.get("max_depth"))
        .and_then(|v| v.as_u64())
        .map(|d| d.clamp(1, 20) as usize)
        .unwrap_or(5);
    let min_confidence = min_confidence_param(&params);
    let pool = get_pool().await?;
    let rows = tests_for(&pool, &corpus_slug, &symbol, max_depth, min_confidence)
        .await
        .map_err(|e| format!("tests: {e}"))?;
    let tests: Vec<Value> = rows
        .iter()
        .map(|t| {
            json!({
                "qualified_name": t.qualified_name,
                "file": t.file,
                "start_line": t.start_line,
                "depth": t.depth,
            })
        })
        .collect();
    Ok(json!({
        "corpus": corpus_slug,
        "symbol": symbol,
        "max_depth": max_depth,
        "min_confidence": min_confidence,
        "count": tests.len(),
        "tests": tests,
    }))
}

/// Change-aware, risk-scored review map (the CLI `ff cortex review` over MCP).
///
/// The daemon shells `git` in the caller-supplied `repo_dir` to derive the
/// changed files + touched line ranges, then scores them against the Cortex
/// graph (`ff_brain::cortex::review`) so an agent knows WHERE TO LOOK FIRST in a
/// diff — a tweak to a function dozens of callers depend on outranks a new
/// private helper. `repo_dir` must be the same checkout that was indexed
/// (`ff cortex index`), since review matches files by absolute path.
pub async fn cortex_review(params: Option<Value>) -> HandlerResult {
    let corpus_slug = params
        .as_ref()
        .and_then(|p| p.get("corpus"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: corpus (the indexed repo slug; \
             list them with cortex_corpora)"
                .to_string()
        })?
        .to_string();
    let repo_dir = params
        .as_ref()
        .and_then(|p| p.get("repo_dir"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing required parameter: repo_dir (absolute path to the git \
             checkout that was indexed)"
                .to_string()
        })?
        .to_string();
    let base = params
        .as_ref()
        .and_then(|p| p.get("base"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let depth = params
        .as_ref()
        .and_then(|p| p.get("depth"))
        .and_then(|v| v.as_u64())
        .map(|d| d.clamp(1, 20) as usize)
        .unwrap_or(5);

    let root = Path::new(&repo_dir);
    if !root.is_dir() {
        return Err(format!(
            "repo_dir does not exist or is not a directory: {repo_dir}"
        ));
    }

    // Changed files (repo-relative) → keep only Cortex-supported source → absolute
    // (the path form stored on content:file nodes).
    let changed_rel = git_changed_files(root, base.as_deref())?;
    let changed_abs: Vec<String> = changed_rel
        .iter()
        .filter(|rel| {
            Path::new(rel)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(cortex::ext_lang)
                .map(|l| cortex::SUPPORTED_LANGS.contains(&l))
                .unwrap_or(false)
        })
        .map(|rel| root.join(rel).to_string_lossy().to_string())
        .collect();

    if changed_abs.is_empty() {
        return Ok(json!({
            "corpus": corpus_slug,
            "base": base,
            "changed_files": 0,
            "note": "no changed Cortex-supported source files to review",
        }));
    }

    // Hunk-level refinement: line ranges the diff touched (working-tree coords =
    // the revision Cortex parsed). Best-effort — fall back to file-level if unread.
    let changed_lines = git_changed_line_ranges(root, base.as_deref()).unwrap_or_default();

    let pool = get_pool().await?;
    let report = cortex::review(
        &pool,
        &corpus_slug,
        &changed_abs,
        depth,
        Some(&changed_lines),
    )
    .await
    .map_err(|e| format!("review: {e}"))?;

    let mut value = serde_json::to_value(&report).map_err(|e| format!("serialize report: {e}"))?;
    if let Value::Object(ref mut map) = value {
        map.insert("corpus".to_string(), json!(corpus_slug));
        map.insert("base".to_string(), json!(base));
        map.insert("depth".to_string(), json!(depth));
    }
    Ok(value)
}

/// Changed files (repo-relative) from `git diff` in `root`. With `base`, the
/// branch's own commits (`base...HEAD`) plus uncommitted edits; without it, just
/// uncommitted work (staged + unstaged + untracked) vs HEAD. Deduped + sorted.
/// (Frontend glue — mirrors the CLI's derivation; the pure diff parsing it feeds
/// lives in `ff_brain::cortex`.)
fn git_changed_files(root: &Path, base: Option<&str>) -> Result<Vec<String>, String> {
    use std::collections::BTreeSet;
    let run = |args: &[&str]| -> Result<Vec<String>, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map_err(|e| format!("run git {}: {e}", args.join(" ")))?;
        if !out.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    };

    let mut files: BTreeSet<String> = BTreeSet::new();
    if let Some(b) = base {
        for f in run(&["diff", "--name-only", &format!("{b}...HEAD")])? {
            files.insert(f);
        }
    }
    for f in run(&["diff", "--name-only", "HEAD"])? {
        files.insert(f);
    }
    for f in run(&["ls-files", "--others", "--exclude-standard"])? {
        files.insert(f);
    }
    Ok(files.into_iter().collect())
}

/// Changed line ranges per file (absolute path → 1-based inclusive `(start,end)`
/// ranges in the WORKING-TREE revision). Single two-dot diff so every range is in
/// one coordinate space. Parsing is shared with the CLI via
/// `ff_brain::cortex::parse_diff_line_ranges`.
fn git_changed_line_ranges(
    root: &Path,
    base: Option<&str>,
) -> Result<HashMap<String, Vec<(u32, u32)>>, String> {
    let mut args = vec!["diff", "--unified=0", "--no-color"];
    let base_spec;
    if let Some(b) = base {
        base_spec = b.to_string();
        args.push(&base_spec);
    } else {
        args.push("HEAD");
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&args)
        .output()
        .map_err(|e| format!("run git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let diff = String::from_utf8_lossy(&out.stdout);
    let by_rel = cortex::parse_diff_line_ranges(&diff);
    Ok(by_rel
        .into_iter()
        .map(|(rel, ranges)| (root.join(rel).to_string_lossy().to_string(), ranges))
        .collect())
}

#[cfg(test)]
mod all_corpora_param_tests {
    use super::{bool_param, symbol_param};
    use serde_json::json;

    #[test]
    fn all_corpora_flag_reads_bool_default_false() {
        assert!(bool_param(
            &Some(json!({"all_corpora": true})),
            "all_corpora",
            false
        ));
        assert!(!bool_param(
            &Some(json!({"all_corpora": false})),
            "all_corpora",
            false
        ));
        // absent → default false
        assert!(!bool_param(
            &Some(json!({"symbol": "x"})),
            "all_corpora",
            false
        ));
        assert!(!bool_param(&None, "all_corpora", false));
    }

    #[test]
    fn symbol_param_requires_symbol_no_corpus() {
        assert_eq!(
            symbol_param(&Some(json!({"symbol": "load_model"}))).unwrap(),
            "load_model"
        );
        assert!(symbol_param(&Some(json!({"corpus": "forge-fleet"}))).is_err());
        assert!(symbol_param(&None).is_err());
    }
}
