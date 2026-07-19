//! Standalone decision/docs-to-Cortex ingestor.
//!
//! Links existing vault notes/docs to existing code/file nodes when the document
//! text names a specific symbol or source path.

use anyhow::Result;
use regex::Regex;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use uuid::Uuid;

const PROVENANCE: &str = "text_match";
const CONFIDENCE: f32 = 0.6;

#[derive(Debug, Clone)]
struct DocNode {
    id: Uuid,
    path: String,
    title: String,
    node_type: String,
    project: Option<String>,
}

#[derive(Debug, Clone)]
struct CodeNode {
    id: Uuid,
    path: String,
    title: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct MatchEvidence {
    matched: String,
    method: &'static str,
}

pub async fn ingest_decisions(pool: &PgPool) -> Result<usize> {
    let docs = read_doc_nodes(pool).await?;
    let chunks = read_doc_chunks(pool, &docs).await?;
    let content_files = read_content_files(pool).await?;
    let code_nodes = read_code_nodes(pool).await?;
    let matchers = build_matchers(&code_nodes, &content_files);

    let mut edge_count = 0usize;
    for doc in docs {
        let text = doc_text(&doc, &chunks, &content_files);
        if text.trim().is_empty() {
            continue;
        }

        let mut seen = BTreeMap::<Uuid, MatchEvidence>::new();
        for matcher in &matchers {
            if matcher.matches(&text) {
                seen.entry(matcher.node_id)
                    .or_insert_with(|| MatchEvidence {
                        matched: matcher.pattern.clone(),
                        method: matcher.method,
                    });
            }
        }

        for (code_id, evidence) in seen {
            add_edge(pool, code_id, doc.id, &evidence).await?;
            edge_count += 1;
        }
    }

    Ok(edge_count)
}

async fn read_doc_nodes(pool: &PgPool) -> Result<Vec<DocNode>> {
    let rows = sqlx::query(
        r#"
        SELECT id, path, title, node_type, project
          FROM brain_vault_nodes
         WHERE valid_until IS NULL
           AND (node_type = 'content:note' OR node_type LIKE 'doc:%')
         ORDER BY path ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| DocNode {
            id: row.get("id"),
            path: row.get("path"),
            title: row.get("title"),
            node_type: row.get("node_type"),
            project: row.try_get("project").ok().flatten(),
        })
        .collect())
}

async fn read_code_nodes(pool: &PgPool) -> Result<Vec<CodeNode>> {
    let rows = sqlx::query(
        r#"
        SELECT id, path, title, node_type
          FROM brain_vault_nodes
         WHERE valid_until IS NULL
           AND node_type IN ('code:function', 'code:struct', 'code:class')
         ORDER BY title ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| CodeNode {
            id: row.get("id"),
            path: row.get("path"),
            title: row.get("title"),
        })
        .collect())
}

async fn read_content_files(pool: &PgPool) -> Result<Vec<CodeNode>> {
    let rows = sqlx::query(
        r#"
        SELECT id, path, title, node_type
          FROM brain_vault_nodes
         WHERE valid_until IS NULL
           AND node_type = 'content:file'
         ORDER BY path ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| CodeNode {
            id: row.get("id"),
            path: row.get("path"),
            title: row.get("title"),
        })
        .collect())
}

async fn read_doc_chunks(pool: &PgPool, docs: &[DocNode]) -> Result<HashMap<String, String>> {
    if docs.is_empty() || !rag_chunks_exists(pool).await? {
        return Ok(HashMap::new());
    }

    let paths: Vec<&str> = docs.iter().map(|doc| doc.path.as_str()).collect();
    let rows = sqlx::query(
        r#"
        SELECT source_path, content
          FROM rag_chunks
         WHERE source_path = ANY($1)
         ORDER BY source_path ASC, chunk_index ASC
        "#,
    )
    .bind(&paths)
    .fetch_all(pool)
    .await?;

    let mut chunks: HashMap<String, String> = HashMap::new();
    for row in rows {
        let source_path: String = row.get("source_path");
        let content: String = row.get("content");
        chunks.entry(source_path).or_default().push_str(&content);
    }
    Ok(chunks)
}

async fn rag_chunks_exists(pool: &PgPool) -> Result<bool> {
    let exists = sqlx::query_scalar("SELECT to_regclass('public.rag_chunks') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    Ok(exists)
}

fn doc_text(doc: &DocNode, chunks: &HashMap<String, String>, content_files: &[CodeNode]) -> String {
    let mut text = String::new();
    text.push_str(&doc.title);
    text.push('\n');
    text.push_str(&doc.path);
    text.push('\n');

    if let Some(body) = chunks.get(&doc.path) {
        text.push_str(body);
        text.push('\n');
    }

    if doc.node_type.starts_with("doc:") {
        if let Some(body) = read_doc_file_body(doc, content_files) {
            text.push_str(&body);
        }
    }

    text
}

fn read_doc_file_body(doc: &DocNode, content_files: &[CodeNode]) -> Option<String> {
    let (slug, rel) = parse_doc_path(&doc.path)?;
    if doc
        .project
        .as_deref()
        .is_some_and(|project| project != slug)
    {
        return None;
    }

    let content_path = content_files
        .iter()
        .find(|file| path_matches_rel(&file.path, rel))
        .map(|file| file.path.as_str())?;
    let body = fs::read_to_string(content_path).ok()?;
    Some(body.chars().take(262_144).collect())
}

fn parse_doc_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("doc://")?;
    let (slug, rel_with_anchor) = rest.split_once('/')?;
    let rel = rel_with_anchor
        .split_once('#')
        .map_or(rel_with_anchor, |(r, _)| r);
    if slug.is_empty() || rel.is_empty() {
        return None;
    }
    Some((slug, rel))
}

fn path_matches_rel(path: &str, rel: &str) -> bool {
    path == rel || path.ends_with(&format!("/{rel}"))
}

#[derive(Debug, Clone)]
struct TextMatcher {
    node_id: Uuid,
    pattern: String,
    method: &'static str,
    regex: Regex,
}

impl TextMatcher {
    fn matches(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }
}

fn build_matchers(code_nodes: &[CodeNode], content_files: &[CodeNode]) -> Vec<TextMatcher> {
    let mut matchers = Vec::new();
    let mut seen = HashSet::<(Uuid, String)>::new();

    for node in code_nodes {
        for pattern in symbol_patterns(&node.title) {
            if seen.insert((node.id, pattern.clone())) {
                matchers.push(TextMatcher {
                    node_id: node.id,
                    pattern: pattern.clone(),
                    method: if pattern == node.title {
                        "symbol_title"
                    } else {
                        "symbol_suffix"
                    },
                    regex: token_regex(&pattern),
                });
            }
        }
        if seen.insert((node.id, node.path.clone())) {
            matchers.push(TextMatcher {
                node_id: node.id,
                pattern: node.path.clone(),
                method: "code_path",
                regex: token_regex(&node.path),
            });
        }
    }

    for file in content_files {
        for pattern in file_patterns(file) {
            if seen.insert((file.id, pattern.clone())) {
                matchers.push(TextMatcher {
                    node_id: file.id,
                    pattern: pattern.clone(),
                    method: "file_path",
                    regex: token_regex(&pattern),
                });
            }
        }
    }

    matchers
}

fn symbol_patterns(title: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    let normalized = title.trim();
    if normalized.is_empty() {
        return Vec::new();
    }

    // The full qualified name is always the most-specific pattern — keep it even if
    // is_specific_symbol's heuristics would reject the (often long) whole path.
    if normalized.contains("::") || is_specific_symbol(normalized, true) {
        out.insert(normalized.to_string());
    }

    let parts: Vec<&str> = normalized.split("::").collect();
    for i in 0..parts.len() {
        let suffix = parts[i..].join("::");
        if is_specific_symbol(&suffix, i > 0) {
            out.insert(suffix);
        }
    }

    if let Some(leaf) = parts.last().copied() {
        if is_specific_symbol(leaf, false) {
            out.insert(leaf.to_string());
        }
    }

    out.into_iter().collect()
}

fn file_patterns(file: &CodeNode) -> Vec<String> {
    let mut out = BTreeSet::new();
    let path = file.path.trim();
    if path.is_empty() {
        return Vec::new();
    }

    out.insert(path.to_string());
    if let Some(idx) = path.find("/crates/") {
        out.insert(path[idx + 1..].to_string());
    }
    if let Some(idx) = path.find("/src/") {
        out.insert(path[idx + 1..].to_string());
    }
    if file.title.len() >= 5 && !is_common_token(&file.title) {
        out.insert(file.title.clone());
    }
    out.into_iter().collect()
}

fn is_specific_symbol(symbol: &str, qualified_suffix: bool) -> bool {
    // A qualified symbol (e.g. `Config::new`) is specific via its prefix — keep it
    // even when the bare leaf would be filtered as common/short.
    if symbol.contains("::") {
        return true;
    }
    let leaf = symbol.rsplit("::").next().unwrap_or(symbol);
    let leaf_len = leaf.chars().filter(|ch| ch.is_ascii_alphanumeric()).count();
    if leaf_len < 3 || is_common_token(leaf) {
        return false;
    }
    qualified_suffix || leaf_len >= 4
}

fn is_common_token(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "add"
            | "all"
            | "api"
            | "app"
            | "arg"
            | "cmd"
            | "config"
            | "data"
            | "db"
            | "doc"
            | "end"
            | "err"
            | "get"
            | "id"
            | "impl"
            | "index"
            | "init"
            | "key"
            | "log"
            | "main"
            | "map"
            | "mod"
            | "new"
            | "node"
            | "path"
            | "read"
            | "run"
            | "set"
            | "sql"
            | "test"
            | "type"
            | "use"
            | "util"
            | "val"
            | "write"
    )
}

fn token_regex(pattern: &str) -> Regex {
    let escaped = regex::escape(pattern);
    Regex::new(&format!(r"(?i)(^|[^A-Za-z0-9_]){escaped}([^A-Za-z0-9_]|$)"))
        .expect("valid text-match regex")
}

async fn add_edge(pool: &PgPool, src: Uuid, dst: Uuid, evidence: &MatchEvidence) -> Result<()> {
    let evidence = json!({
        "matched": evidence.matched,
        "method": evidence.method,
    });

    sqlx::query(
        r#"
        INSERT INTO brain_vault_edges
            (src_id, dst_id, edge_type, provenance, confidence, method, evidence)
        VALUES ($1, $2, 'documented_by', $3, $4, $5, $6)
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance,
              method = COALESCE(EXCLUDED.method, brain_vault_edges.method),
              evidence = COALESCE(EXCLUDED.evidence, brain_vault_edges.evidence)
        "#,
    )
    .bind(src)
    .bind(dst)
    .bind(PROVENANCE)
    .bind(CONFIDENCE)
    .bind(evidence["method"].as_str())
    .bind(&evidence)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_patterns_skip_common_leaves_but_keep_qualified_suffixes() {
        let patterns = symbol_patterns("ff::cortex::Config::new");
        assert!(patterns.contains(&"ff::cortex::Config::new".to_string()));
        assert!(patterns.contains(&"Config::new".to_string()));
        assert!(!patterns.contains(&"new".to_string()));
    }

    #[test]
    fn doc_path_parses_sections() {
        assert_eq!(
            parse_doc_path("doc://fleet/docs/plan.md#section"),
            Some(("fleet", "docs/plan.md"))
        );
    }
}
