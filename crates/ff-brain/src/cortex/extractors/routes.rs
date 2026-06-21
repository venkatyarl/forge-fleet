use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::{Result, anyhow};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct RoutesExtractor;

#[derive(Debug, Clone)]
struct RouteSite {
    method: String,
    path: String,
    handler: String,
    line: i32,
}

#[derive(Debug, Clone)]
struct FunctionSpan {
    path: String,
    start_line: i32,
    end_line: i32,
}

#[derive(Debug, Clone)]
struct HandlerFunction {
    title: String,
    path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EndpointRow {
    pub method: String,
    pub path: String,
    pub corpus: String,
    pub handler: Option<String>,
    pub handler_path: Option<String>,
    pub guarded_by: Vec<String>,
    pub candidate_unauthenticated: bool,
    pub confidence: Option<f32>,
}

#[async_trait::async_trait]
impl Extractor for RoutesExtractor {
    fn name(&self) -> &'static str {
        "routes"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let handlers = load_handler_functions(ctx).await?;
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root, "rs")? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let file_path = path.to_string_lossy().to_string();
                let functions = function_spans(ctx, &file_path).await?;
                for site in find_route_sites(&source) {
                    let registered_in = enclosing_function(&functions, site.line);
                    let handler = resolve_handler(&handlers, &site.handler);
                    facts.add_route(site, handler, registered_in);
                }
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    endpoints: BTreeMap<String, (String, String)>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            endpoints: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_route(
        &mut self,
        site: RouteSite,
        handler: Option<&HandlerFunction>,
        registered_in: Option<&FunctionSpan>,
    ) {
        let endpoint_path = endpoint_node_path(self.corpus, &site.method, &site.path);
        self.endpoints.insert(
            endpoint_path.clone(),
            (site.method.clone(), site.path.clone()),
        );

        let Some(handler) = handler else {
            return;
        };

        let key = (
            endpoint_path.clone(),
            handler.path.clone(),
            "serves".to_string(),
        );
        if !self.seen_edges.insert(key) {
            return;
        }

        self.edges.push(Fact::Edge {
            src_path: endpoint_path,
            dst_path: handler.path.clone(),
            edge_type: "serves".to_string(),
            confidence: 0.9,
            provenance: "ast".to_string(),
            method: Some(site.method.clone()),
            evidence: Some(json!({
                "line": site.line,
                "handler": site.handler,
                "registered_in": registered_in.map(|f| f.path.clone()),
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for (path, (method, route_path)) in &self.endpoints {
            out.push(Fact::Node {
                path: path.clone(),
                title: format!("{method} {route_path}"),
                node_type: "http:endpoint".to_string(),
                start_line: None,
                end_line: None,
                confidence: 0.9,
                provenance: "ast".to_string(),
            });
        }
        out.extend(self.edges);
        out
    }
}

async fn load_handler_functions(ctx: &ExtractCtx<'_>) -> Result<Vec<HandlerFunction>> {
    let rows = sqlx::query(
        r#"SELECT title, path
             FROM brain_vault_nodes
            WHERE project = $1
              AND node_type = 'code:function'
              AND valid_until IS NULL
            ORDER BY title COLLATE "C""#,
    )
    .bind(ctx.corpus_slug)
    .fetch_all(ctx.pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| HandlerFunction {
            title: r.get("title"),
            path: r.get("path"),
        })
        .collect())
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

fn find_route_sites(source: &str) -> Vec<RouteSite> {
    let line_starts = line_start_offsets(source);
    let mut sites = Vec::new();
    collect_route_sites(source, "", &line_starts, 0, &mut sites);
    sites.sort_by_key(|s| (s.path.clone(), s.method.clone(), s.handler.clone()));
    sites
}

fn collect_route_sites(
    source: &str,
    prefix: &str,
    line_starts: &[usize],
    base_offset: usize,
    sites: &mut Vec<RouteSite>,
) {
    let mut offset = 0usize;
    while let Some(pos) = source[offset..].find(".route(") {
        let at = offset + pos;
        let open = at + ".route".len();
        if let Some((inside, close)) = balanced_parens(source, open) {
            let args = top_level_args(inside);
            if args.len() >= 2 {
                if let Some(route_path) = parse_string_literal(args[0].trim()) {
                    let full_path = join_paths(prefix, &route_path);
                    for (method, handler) in method_handlers(args[1]) {
                        sites.push(RouteSite {
                            method,
                            path: full_path.clone(),
                            handler,
                            line: byte_to_line(line_starts, base_offset + at),
                        });
                    }
                }
            }
            offset = close + 1;
        } else {
            offset = at + ".route(".len();
        }
    }

    offset = 0;
    while let Some(pos) = source[offset..].find(".nest(") {
        let at = offset + pos;
        let open = at + ".nest".len();
        if let Some((inside, close)) = balanced_parens(source, open) {
            let args = top_level_args(inside);
            if args.len() >= 2 {
                if let Some(nest_path) = parse_string_literal(args[0].trim()) {
                    let nested_prefix = join_paths(prefix, &nest_path);
                    collect_route_sites(
                        args[1],
                        &nested_prefix,
                        line_starts,
                        base_offset + at + source[at..].find(args[1]).unwrap_or(0),
                        sites,
                    );
                }
            }
            offset = close + 1;
        } else {
            offset = at + ".nest(".len();
        }
    }
}

fn method_handlers(expr: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for method in ["get", "post", "put", "delete", "patch"] {
        let mut offset = 0usize;
        while let Some(pos) = expr[offset..].find(method) {
            let at = offset + pos;
            let before = expr[..at].chars().next_back();
            let after = expr[at + method.len()..].chars().next();
            if before.is_none_or(|ch| !is_ident_char(ch))
                && after.is_some_and(|ch| ch == '(')
                && let Some((inside, close)) = balanced_parens(expr, at + method.len())
            {
                if let Some(first) = top_level_args(inside).first() {
                    if let Some(handler) = handler_path(first) {
                        out.push((method.to_ascii_uppercase(), handler));
                    }
                }
                offset = close + 1;
                continue;
            }
            offset = at + method.len();
        }
    }
    out
}

fn handler_path(expr: &str) -> Option<String> {
    let expr = expr.trim();
    let mut end = 0usize;
    for (idx, ch) in expr.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    Some(expr[..end].trim_matches(':').to_string())
}

fn resolve_handler<'a>(
    handlers: &'a [HandlerFunction],
    raw_handler: &str,
) -> Option<&'a HandlerFunction> {
    let raw = raw_handler.trim();
    if raw.is_empty() {
        return None;
    }

    let mut matches = handlers
        .iter()
        .filter(|h| handler_matches(&h.title, raw))
        .collect::<Vec<_>>();
    matches.sort_by_key(|h| (h.title.len(), h.title.clone()));
    matches.into_iter().next()
}

fn handler_matches(title: &str, raw: &str) -> bool {
    if title == raw {
        return true;
    }
    if let Some(rest) = raw.strip_prefix("crate::") {
        return title.ends_with(&format!("::{rest}"));
    }
    if raw.contains("::") {
        return title.ends_with(&format!("::{raw}"));
    }
    title.rsplit("::").next() == Some(raw)
}

fn top_level_args(input: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut string = false;
    let mut escape = false;
    for (idx, ch) in input.char_indices() {
        if string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                string = false;
            }
            continue;
        }
        match ch {
            '"' => string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(input[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < input.len() {
        args.push(input[start..].trim());
    }
    args
}

fn balanced_parens(source: &str, open: usize) -> Option<(&str, usize)> {
    if source.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut string = false;
    let mut escape = false;
    for (idx, ch) in source[open..].char_indices() {
        let abs = open + idx;
        if string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                string = false;
            }
            continue;
        }
        match ch {
            '"' => string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&source[open + 1..abs], abs));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_string_literal(input: &str) -> Option<String> {
    let input = input.trim();
    if !input.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut escape = false;
    for ch in input[1..].chars() {
        if escape {
            out.push(ch);
            escape = false;
        } else if ch == '\\' {
            escape = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn join_paths(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if prefix.is_empty() {
        format!("/{path}")
    } else if path.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}/{path}")
    }
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

fn endpoint_node_path(corpus: &str, method: &str, path: &str) -> String {
    format!("http://{corpus}/{method}{path}")
}

pub async fn print_endpoints_command(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    path: Option<&str>,
    format: &str,
) -> Result<()> {
    if let Some(path) = path {
        let rows = endpoint(pool, corpus_slug, path).await?;
        print_endpoint_rows(&rows, format, path);
    } else {
        let rows = endpoints(pool, corpus_slug).await?;
        print_endpoint_rows(&rows, format, "endpoints");
    }
    Ok(())
}

pub async fn endpoints(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<EndpointRow>> {
    endpoint_rows(pool, corpus_slug, None).await
}

pub async fn endpoint(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    path: &str,
) -> Result<Vec<EndpointRow>> {
    let rows = endpoint_rows(pool, corpus_slug, Some(path)).await?;
    if rows.is_empty() {
        return Err(anyhow!("no http endpoint matching '{path}' in cortex"));
    }
    Ok(rows)
}

async fn endpoint_rows(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    endpoint_sel: Option<&str>,
) -> Result<Vec<EndpointRow>> {
    let rows = sqlx::query(
        r#"SELECT ep.id AS endpoint_id,
                  ep.title AS endpoint_title,
                  ep.project AS corpus,
                  ep.confidence AS confidence,
                  h.title AS handler,
                  h.path AS handler_path,
                  g.title AS gate
             FROM brain_vault_nodes ep
             LEFT JOIN brain_vault_edges se
                    ON se.src_id = ep.id
                   AND se.edge_type = 'serves'
                   AND COALESCE(se.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = ep.project), 0)
                   )
             LEFT JOIN brain_vault_nodes h
                    ON h.id = se.dst_id
                   AND h.node_type = 'code:function'
                   AND h.valid_until IS NULL
                   AND COALESCE(h.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = ep.project), 0)
                   )
             LEFT JOIN brain_vault_edges ge
                    ON ge.src_id = h.id
                   AND ge.edge_type = 'guarded_by'
                   AND COALESCE(ge.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = ep.project), 0)
                   )
             LEFT JOIN brain_vault_nodes g
                    ON g.id = ge.dst_id
                   AND g.node_type = 'security:gate'
                   AND COALESCE(g.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = ep.project), 0)
                   )
            WHERE ep.node_type = 'http:endpoint'
              AND ep.valid_until IS NULL
              AND ($1::text IS NULL OR ep.project = $1)
              AND COALESCE(ep.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = ep.project), 0)
              )
              AND (
                  $2::text IS NULL
                  OR ep.title = $2
                  OR split_part(ep.title, ' ', 2) = $2
                  OR ep.path LIKE ('http://%/' || $2)
              )
            ORDER BY ep.project, ep.title, h.title, g.title"#,
    )
    .bind(corpus_slug)
    .bind(endpoint_sel)
    .fetch_all(pool)
    .await?;

    let mut out: BTreeMap<Uuid, EndpointRow> = BTreeMap::new();
    for r in rows {
        let endpoint_id: Uuid = r.get("endpoint_id");
        let title: String = r.get("endpoint_title");
        let (method, path) = split_endpoint_title(&title);
        let gate: Option<String> = r.get("gate");
        let row = out.entry(endpoint_id).or_insert_with(|| EndpointRow {
            method,
            path,
            corpus: r.get("corpus"),
            handler: r.get("handler"),
            handler_path: r.get("handler_path"),
            guarded_by: Vec::new(),
            candidate_unauthenticated: false,
            confidence: r.get("confidence"),
        });
        if let Some(gate) = gate {
            if !row.guarded_by.contains(&gate) {
                row.guarded_by.push(gate);
            }
        }
    }

    let mut rows = out.into_values().collect::<Vec<_>>();
    for row in &mut rows {
        row.guarded_by.sort();
        row.candidate_unauthenticated = row.handler.is_some() && row.guarded_by.is_empty();
    }
    rows.sort_by_key(|r| (r.corpus.clone(), r.method.clone(), r.path.clone()));
    Ok(rows)
}

fn split_endpoint_title(title: &str) -> (String, String) {
    let mut parts = title.splitn(2, ' ');
    (
        parts.next().unwrap_or_default().to_string(),
        parts.next().unwrap_or_default().to_string(),
    )
}

fn print_endpoint_rows(rows: &[EndpointRow], format: &str, label: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{} {}", row.method, row.path);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no http:endpoint nodes in cortex (run `ff cortex index`?)");
                return;
            }
            println!("cortex {label} - {} endpoint(s):", rows.len());
            println!(
                "  {:<6} {:<42} {:<48} {:<12} corpus",
                "method", "path", "handler", "auth"
            );
            for row in rows {
                let auth = if row.candidate_unauthenticated {
                    "candidate"
                } else if row.guarded_by.is_empty() {
                    "-"
                } else {
                    "guarded"
                };
                println!(
                    "  {:<6} {:<42} {:<48} {:<12} {}",
                    row.method,
                    truncate(&row.path, 42),
                    truncate(row.handler.as_deref().unwrap_or("-"), 48),
                    auth,
                    row.corpus
                );
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = s.chars().take(max.saturating_sub(1)).collect::<String>();
    out.push('~');
    out
}
