//! `ff brain corpus …` + `ff brain query …` handlers. Thin dispatch into
//! ff_brain::corpus, reusing the cached fleet PgPool passed in by brain_cmd.

use crate::{CYAN, RESET};
use anyhow::{anyhow, Result};
use ff_brain::corpus;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn handle_corpus(pool: &PgPool, cmd: crate::CorpusCommand) -> Result<()> {
    match cmd {
        crate::CorpusCommand::Add { slug, title, roots, labels } => {
            let title = title.unwrap_or_else(|| slug.clone());
            let pairs: Vec<(String, Option<String>)> = roots.iter().enumerate()
                .map(|(i, r)| (r.clone(), labels.get(i).cloned())).collect();
            let c = corpus::add_corpus(pool, &slug, &title, &pairs).await?;
            let sources = corpus::list_sources(pool, &c).await?;
            println!("{CYAN}\u{2713} Corpus '{}' ({}){RESET}", c.slug, c.title);
            for s in &sources {
                println!("  source: {} [{}]", s.root_path, s.label.as_deref().unwrap_or("-"));
            }
            println!("  ({} source root(s) unified under one corpus)", sources.len());
        }
        crate::CorpusCommand::SourceAdd { slug, root, label } => {
            let c = require_corpus(pool, &slug).await?;
            let s = corpus::add_source(pool, &c, &root, label.as_deref()).await?;
            println!("{CYAN}\u{2713} source added:{RESET} {}", s.root_path);
        }
        crate::CorpusCommand::Scan { slug, source, max_depth, apply } => {
            let c = require_corpus(pool, &slug).await?;
            println!("{CYAN}\u{25b6} Scanning corpus '{}'\u{2026}{RESET}", c.slug);
            let report = corpus::scan(pool, &c, source.as_deref(), max_depth).await?;
            println!("  sources scanned: {}", report.sources_scanned);
            println!("  dirs:            {}", report.dirs);
            println!("  files:           {}", report.files);
            println!("  content nodes:   {}", report.nodes_upserted);
            println!("  contains edges:  {}", report.edges);
            println!("  candidates:      {}", report.candidates);
            if apply {
                let n = corpus::confirm_candidates(pool, &c, &[], true, 0.8).await?;
                println!("{CYAN}  auto-confirmed:  {n} (confidence >= 0.8){RESET}");
            } else {
                println!("  (dry-run: review with `ff brain corpus candidates {slug}`)");
            }
            println!("{CYAN}\u{2713} Done{RESET}");
        }
        crate::CorpusCommand::List { format } => {
            let rows = corpus::list_corpora(pool).await?;
            if format == "json" {
                let v: Vec<_> = rows.iter().map(|r| serde_json::json!({
                    "slug": r.slug, "title": r.title, "sources": r.sources,
                    "entities": r.entities, "facets": r.facets, "content": r.content
                })).collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{:<20} {:<22} {:>7} {:>8} {:>6} {:>8}",
                    "SLUG", "TITLE", "SOURCES", "ENTITIES", "FACETS", "CONTENT");
                for r in &rows {
                    println!("{:<20} {:<22} {:>7} {:>8} {:>6} {:>8}",
                        r.slug, r.title, r.sources, r.entities, r.facets, r.content);
                }
            }
        }
        crate::CorpusCommand::Sources { slug, format } => {
            let c = require_corpus(pool, &slug).await?;
            let rows = corpus::list_sources(pool, &c).await?;
            if format == "json" {
                let v: Vec<_> = rows.iter().map(|s| serde_json::json!({
                    "root_path": s.root_path, "label": s.label,
                    "scan_status": s.scan_status, "file_count": s.file_count
                })).collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for s in &rows {
                    println!("{:<10} {:<10} {:>6}  {}",
                        s.label.as_deref().unwrap_or("-"), s.scan_status, s.file_count, s.root_path);
                }
            }
        }
        crate::CorpusCommand::Candidates { slug, status, kind, format } => {
            let c = require_corpus(pool, &slug).await?;
            let rows = corpus::list_candidates(pool, &c, status.as_deref(), kind.as_deref()).await?;
            if format == "json" {
                let v: Vec<_> = rows.iter().map(|r| serde_json::json!({
                    "id": r.id, "kind": r.kind, "title": r.title,
                    "heuristic": r.heuristic, "confidence": r.confidence,
                    "status": r.status, "payload": r.payload
                })).collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{:<38} {:<16} {:<5} {:<14} {}", "ID", "KIND", "CONF", "HEURISTIC", "TITLE");
                for r in &rows {
                    println!("{:<38} {:<16} {:<5.2} {:<14} {}",
                        r.id, r.kind, r.confidence, r.heuristic.as_deref().unwrap_or("-"), r.title);
                }
            }
        }
        crate::CorpusCommand::Confirm { slug, candidates, all } => {
            let c = require_corpus(pool, &slug).await?;
            let ids = parse_uuids(&candidates)?;
            let n = corpus::confirm_candidates(pool, &c, &ids, all, 0.0).await?;
            println!("{CYAN}\u{2713} confirmed {n} candidate(s){RESET}");
        }
        crate::CorpusCommand::Reject { slug, candidates } => {
            let _ = require_corpus(pool, &slug).await?;
            let ids = parse_uuids(&candidates)?;
            let n = corpus::reject_candidates(pool, &ids).await?;
            println!("{CYAN}\u{2713} rejected {n} candidate(s){RESET}");
        }
        crate::CorpusCommand::Query {
            slug, entities, products, roles, statuses, modalities, facets, format,
        } => {
            handle_query(pool, &slug, &entities, &products, &roles, &statuses, &modalities, &facets, &format).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_query(
    pool: &PgPool, slug: &str, entities: &[String], products: &[String],
    roles: &[String], statuses: &[String], modalities: &[String],
    facets: &[String], format: &str,
) -> Result<()> {
    let c = require_corpus(pool, slug).await?;
    let mut q = corpus::FacetQuery::default();
    q.entities.extend(entities.iter().cloned());
    q.entities.extend(products.iter().cloned());
    for r in roles { q.facets.push(("role".into(), r.clone())); }
    for s in statuses { q.facets.push(("status".into(), s.clone())); }
    for m in modalities { q.facets.push(("modality".into(), m.clone())); }
    for f in facets {
        if let Some((dim, val)) = f.split_once(':') {
            q.facets.push((dim.to_string(), val.to_string()));
        }
    }

    let rows = corpus::query(pool, &c, &q).await?;
    match format {
        "json" => {
            let v: Vec<_> = rows.iter().map(|r| serde_json::json!({
                "id": r.id, "path": r.path, "title": r.title, "node_type": r.node_type
            })).collect();
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        "paths" => { for r in &rows { println!("{}", r.path); } }
        _ => {
            println!("{CYAN}{} result(s):{RESET}", rows.len());
            for r in &rows { println!("  [{}] {}", r.node_type, r.path); }
        }
    }
    Ok(())
}

async fn require_corpus(pool: &PgPool, slug: &str) -> Result<corpus::Corpus> {
    corpus::get_corpus(pool, slug).await?
        .ok_or_else(|| anyhow!("no corpus with slug '{slug}' (create it with `ff brain corpus add`)"))
}

fn parse_uuids(raw: &[String]) -> Result<Vec<Uuid>> {
    raw.iter()
        .map(|s| Uuid::parse_str(s).map_err(|e| anyhow!("bad candidate id '{s}': {e}")))
        .collect()
}
