//! Cortex code-graph MCP tools — let fleet agents consume the pre-indexed code
//! graph the way they consume the CRG MCP server (the "CodeGraph" pattern:
//! a local graph served over MCP, fewer tokens / tool-calls than file scanning).
//!
//! All read-only: `cortex_corpora` (discover indexed repos), `cortex_callers`,
//! `cortex_callees`, and `cortex_impact` (transitive blast radius). The graph is
//! built by `ff cortex index`; these tools only query it.

use ff_brain::{callees, callers, corpus, cortex, find_symbols, find_symbols_semantic, impact};
use ff_core::config;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

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
