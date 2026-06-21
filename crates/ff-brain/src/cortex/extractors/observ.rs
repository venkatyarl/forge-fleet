use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct ObservExtractor;

#[derive(Debug, Clone)]
struct FunctionSpan {
    path: String,
    start_line: i32,
    end_line: i32,
}

#[derive(Debug, Clone)]
struct LogSite {
    level: String,
    line: i32,
    call: String,
}

#[derive(Debug, Clone)]
struct ErrorTypeSite {
    name: String,
    file_path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ErrorType {
    pub name: String,
    pub corpus: String,
    pub path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogLevelSummary {
    pub level: String,
    pub corpus: String,
    pub emits: i64,
    pub error_functions: Vec<String>,
}

#[async_trait::async_trait]
impl Extractor for ObservExtractor {
    fn name(&self) -> &'static str {
        "observ"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root, "rs")? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let file_path = path.to_string_lossy().to_string();
                let functions = function_spans(ctx, &file_path).await?;

                for site in find_log_sites(&source) {
                    let Some(function) = enclosing_function(&functions, site.line) else {
                        continue;
                    };
                    facts.add_log(function.path.clone(), site);
                }

                for site in find_error_types(&source, &file_path) {
                    facts.add_error(site);
                }
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    levels: BTreeMap<String, f32>,
    errors: BTreeMap<String, ErrorTypeSite>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            levels: BTreeMap::new(),
            errors: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_log(&mut self, src_path: String, site: LogSite) {
        self.levels
            .entry(site.level.clone())
            .and_modify(|confidence| *confidence = confidence.max(0.9))
            .or_insert(0.9);

        let dst_path = level_path(self.corpus, &site.level);
        let key = (src_path.clone(), dst_path.clone(), "logs".to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path,
            edge_type: "logs".to_string(),
            confidence: 0.9,
            provenance: "ast".to_string(),
            method: Some("EXTRACTED".to_string()),
            evidence: Some(json!({
                "line": site.line,
                "call": site.call,
                "level": site.level,
            })),
        });
    }

    fn add_error(&mut self, site: ErrorTypeSite) {
        self.errors.entry(site.name.clone()).or_insert(site);
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for (level, confidence) in &self.levels {
            out.push(Fact::Node {
                path: level_path(self.corpus, level),
                title: format!("obs:level/{level}"),
                node_type: "obs:level".to_string(),
                start_line: None,
                end_line: None,
                confidence: *confidence,
                provenance: "ast".to_string(),
            });
        }
        for site in self.errors.values() {
            out.push(Fact::Node {
                path: error_path(self.corpus, &site.name),
                title: site.name.clone(),
                node_type: "error:type".to_string(),
                start_line: None,
                end_line: None,
                confidence: 0.8,
                provenance: "ast".to_string(),
            });
            out.push(Fact::Edge {
                src_path: site.file_path.clone(),
                dst_path: error_path(self.corpus, &site.name),
                edge_type: "defines_error".to_string(),
                confidence: 0.8,
                provenance: "ast".to_string(),
                method: Some("EXTRACTED".to_string()),
                evidence: Some(json!({ "type": site.name })),
            });
        }
        out.extend(self.edges);
        out
    }
}

async fn function_spans(ctx: &ExtractCtx<'_>, file_path: &str) -> Result<Vec<FunctionSpan>> {
    let rows = sqlx::query(
        r#"WITH RECURSIVE down(id) AS (
               SELECT dst_id
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes f ON f.id = e.src_id
                WHERE f.project = $1
                  AND f.node_type = 'content:file'
                  AND f.path = $2
                  AND e.edge_type = 'contains'
               UNION
               SELECT e.dst_id
                 FROM brain_vault_edges e
                 JOIN down d ON d.id = e.src_id
                WHERE e.edge_type = 'contains'
           )
           SELECT n.path, n.start_line, n.end_line
             FROM down
             JOIN brain_vault_nodes n ON n.id = down.id
            WHERE n.node_type = 'code:function'
              AND n.start_line IS NOT NULL
              AND n.end_line IS NOT NULL
            ORDER BY n.start_line DESC,
                     (n.end_line - n.start_line) ASC"#,
    )
    .bind(ctx.corpus_slug)
    .bind(file_path)
    .fetch_all(ctx.pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| FunctionSpan {
            path: r.get("path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
        })
        .collect())
}

fn enclosing_function(functions: &[FunctionSpan], line: i32) -> Option<&FunctionSpan> {
    functions
        .iter()
        .filter(|f| f.start_line <= line && line <= f.end_line)
        .min_by_key(|f| f.end_line - f.start_line)
}

fn find_log_sites(source: &str) -> Vec<LogSite> {
    let line_starts = line_start_offsets(source);
    let mut sites = Vec::new();

    for level in ["info", "warn", "error", "debug", "trace"] {
        for macro_name in [
            format!("{level}!"),
            format!("tracing::{level}!"),
            format!("log::{level}!"),
        ] {
            for at in find_macro_occurrences(source, &macro_name) {
                sites.push(LogSite {
                    level: level.to_string(),
                    line: byte_to_line(&line_starts, at),
                    call: macro_name.clone(),
                });
            }
        }
    }

    for at in find_macro_occurrences(source, "tracing::event!") {
        if let Some(level) = tracing_event_level(source, at) {
            sites.push(LogSite {
                level,
                line: byte_to_line(&line_starts, at),
                call: "tracing::event!".to_string(),
            });
        }
    }

    sites.sort_by_key(|s| (s.line, s.level.clone(), s.call.clone()));
    sites
}

fn find_error_types(source: &str, file_path: &str) -> Vec<ErrorTypeSite> {
    let item_re = Regex::new(
        r#"(?ms)(?P<attrs>(?:\s*#\[[^\]]+\]\s*)*)\b(?:pub(?:\([^)]*\))?\s+)?(?P<kind>enum|struct)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)"#,
    )
    .expect("valid rust item regex");
    let mut sites = Vec::new();
    for cap in item_re.captures_iter(source) {
        let Some(name) = cap.name("name").map(|m| m.as_str()) else {
            continue;
        };
        let derives_error = cap
            .name("attrs")
            .map(|m| m.as_str().contains("thiserror::Error") || m.as_str().contains("Error"))
            .unwrap_or(false);
        if derives_error || name.ends_with("Error") {
            sites.push(ErrorTypeSite {
                name: name.to_string(),
                file_path: file_path.to_string(),
            });
        }
    }
    sites
}

fn find_macro_occurrences(source: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while let Some(pos) = source[offset..].find(needle) {
        let at = offset + pos;
        let before = source[..at].chars().next_back();
        let after = source[at + needle.len()..].chars().next();
        let before_ok = before.is_none_or(|ch| !is_ident_char(ch) && ch != ':');
        let after_ok = after.is_none_or(|ch| !is_ident_char(ch));
        if before_ok && after_ok {
            out.push(at);
        }
        offset = at + needle.len();
    }
    out
}

fn tracing_event_level(source: &str, at: usize) -> Option<String> {
    let open = source[at..].find('(')? + at;
    let close = matching_paren(source, open)?;
    let args = &source[open + 1..close];
    for level in ["info", "warn", "error", "debug", "trace"] {
        let upper = level.to_ascii_uppercase();
        if args.contains(&format!("Level::{upper}"))
            || args.contains(&format!("tracing::Level::{upper}"))
            || args.contains(&format!("log::Level::{upper}"))
        {
            return Some(level.to_string());
        }
    }
    None
}

fn matching_paren(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in source[open..].char_indices() {
        let at = open + i;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn byte_to_line(line_starts: &[usize], byte: usize) -> i32 {
    line_starts.partition_point(|&s| s <= byte).max(1) as i32
}

fn collect_files(root: &Path, ext: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, ext, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(path: &Path, ext: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    if matches!(name, ".git" | "target" | "node_modules" | ".direnv") {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        collect_files_inner(&entry?.path(), ext, out)?;
    }
    Ok(())
}

fn level_path(corpus: &str, level: &str) -> String {
    format!("obs://{corpus}/level/{level}")
}

fn error_path(corpus: &str, name: &str) -> String {
    format!("err://{corpus}/{name}")
}

pub async fn errors(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<ErrorType>> {
    let rows = sqlx::query(
        r#"SELECT title, project, path
             FROM brain_vault_nodes
            WHERE node_type = 'error:type'
              AND ($1::text IS NULL OR project = $1)
              AND COALESCE(generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = brain_vault_nodes.project), 0)
              )
            ORDER BY project, title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ErrorType {
            name: r.get("title"),
            corpus: r.get("project"),
            path: r.get("path"),
        })
        .collect())
}

pub async fn logs(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<LogLevelSummary>> {
    let rows = sqlx::query(
        r#"SELECT l.title AS level_title,
                  l.project AS corpus,
                  COUNT(DISTINCT e.src_id) AS emits,
                  COALESCE(
                      ARRAY_AGG(DISTINCT f.title ORDER BY f.title)
                          FILTER (WHERE l.path LIKE '%/level/error'),
                      ARRAY[]::text[]
                  ) AS error_functions
             FROM brain_vault_nodes l
             LEFT JOIN brain_vault_edges e
                    ON e.dst_id = l.id
                   AND e.edge_type = 'logs'
                   AND COALESCE(e.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = l.project), 0)
                   )
             LEFT JOIN brain_vault_nodes f
                    ON f.id = e.src_id
                   AND f.node_type = 'code:function'
            WHERE l.node_type = 'obs:level'
              AND ($1::text IS NULL OR l.project = $1)
              AND COALESCE(l.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = l.project), 0)
              )
            GROUP BY l.title, l.project, l.path
            ORDER BY l.project, l.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let title: String = r.get("level_title");
            LogLevelSummary {
                level: title
                    .strip_prefix("obs:level/")
                    .unwrap_or(title.as_str())
                    .to_string(),
                corpus: r.get("corpus"),
                emits: r.get("emits"),
                error_functions: r.get("error_functions"),
            }
        })
        .collect())
}
