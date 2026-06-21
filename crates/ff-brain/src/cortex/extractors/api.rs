use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

pub struct ApiExtractor;

#[derive(Debug, Clone)]
struct CodeFn {
    path: String,
    file_path: String,
    start_line: i32,
    end_line: i32,
}

#[derive(Debug, Clone)]
struct CodeType {
    path: String,
    title: String,
}

#[derive(Debug, Clone)]
struct ApiSite {
    extractor: &'static str,
    typ: String,
    line: i32,
}

#[derive(Debug, Clone)]
struct ExternalCall {
    service: String,
    url: Option<String>,
    confidence: f32,
    line: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiHandlerSummary {
    pub handler: String,
    pub corpus: String,
    pub accepts: Vec<String>,
    pub returns: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExternalServiceSummary {
    pub service: String,
    pub corpus: String,
    pub callers: Vec<String>,
}

#[async_trait::async_trait]
impl Extractor for ApiExtractor {
    fn name(&self) -> &'static str {
        "api"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let code_fns = load_code_functions(ctx.pool, ctx.corpus_slug).await?;
        let code_types = load_code_types(ctx.pool, ctx.corpus_slug).await?;
        if code_fns.is_empty() {
            return Ok(Vec::new());
        }

        let mut functions_by_file: HashMap<String, Vec<CodeFn>> = HashMap::new();
        for function in code_fns {
            functions_by_file
                .entry(function.file_path.clone())
                .or_default()
                .push(function);
        }
        for functions in functions_by_file.values_mut() {
            functions.sort_by_key(|f| (f.start_line, f.end_line));
        }

        let type_resolver = TypeResolver::new(code_types);
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root)? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let file_path = path.to_string_lossy().to_string();
                let Some(file_fns) = functions_by_file.get(&file_path) else {
                    continue;
                };
                let Some(parsed) = parse_rust_file(&source) else {
                    continue;
                };
                let line_starts = line_start_offsets(&source);
                walk_functions(&parsed, source.as_bytes(), &line_starts, &mut |node| {
                    let start_line = byte_to_line(&line_starts, node.start_byte());
                    let end_line = byte_to_line(&line_starts, node.end_byte());
                    let Some(function) = resolve_function(file_fns, start_line, end_line) else {
                        return;
                    };
                    let signature = function_signature(node, source.as_bytes());
                    for site in request_dtos(signature, &line_starts, node.start_byte()) {
                        if let Some(dto) = type_resolver.resolve(&site.typ) {
                            facts.add_edge(
                                function.path.clone(),
                                dto.path.clone(),
                                "accepts",
                                0.9,
                                json!({
                                    "extractor": site.extractor,
                                    "type": site.typ,
                                    "line": site.line,
                                }),
                            );
                        }
                    }
                    for site in response_dtos(signature, &line_starts, node.start_byte()) {
                        if let Some(dto) = type_resolver.resolve(&site.typ) {
                            facts.add_edge(
                                function.path.clone(),
                                dto.path.clone(),
                                "returns",
                                0.85,
                                json!({
                                    "extractor": site.extractor,
                                    "type": site.typ,
                                    "line": site.line,
                                }),
                            );
                        }
                    }
                    let body = node
                        .child_by_field_name("body")
                        .map(|body| node_text(&body, source.as_bytes()))
                        .unwrap_or_default();
                    for call in external_calls(body, &line_starts, node.start_byte()) {
                        facts.add_external_call(function.path.clone(), call);
                    }
                });
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    services: BTreeMap<String, f32>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            services: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_edge(
        &mut self,
        src_path: String,
        dst_path: String,
        edge_type: &'static str,
        confidence: f32,
        evidence: serde_json::Value,
    ) {
        let key = (src_path.clone(), dst_path.clone(), edge_type.to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path,
            edge_type: edge_type.to_string(),
            confidence,
            provenance: "ast".to_string(),
            method: Some("SIGNATURE".to_string()),
            evidence: Some(evidence),
        });
    }

    fn add_external_call(&mut self, src_path: String, call: ExternalCall) {
        self.services
            .entry(call.service.clone())
            .and_modify(|confidence| *confidence = confidence.max(call.confidence))
            .or_insert(call.confidence);
        let dst_path = service_path(self.corpus, &call.service);
        let key = (
            src_path.clone(),
            dst_path.clone(),
            "calls_external".to_string(),
        );
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path,
            edge_type: "calls_external".to_string(),
            confidence: call.confidence,
            provenance: "ast".to_string(),
            method: Some("HTTP_CALL".to_string()),
            evidence: Some(json!({
                "line": call.line,
                "url": call.url,
                "service": call.service,
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for (service, confidence) in &self.services {
            out.push(Fact::Node {
                path: service_path(self.corpus, service),
                title: service.clone(),
                node_type: "ext:service".to_string(),
                start_line: None,
                end_line: None,
                confidence: *confidence,
                provenance: "ast".to_string(),
            });
        }
        out.extend(self.edges);
        out
    }
}

struct TypeResolver {
    by_leaf: HashMap<String, Vec<CodeType>>,
    by_title: HashMap<String, CodeType>,
}

impl TypeResolver {
    fn new(types: Vec<CodeType>) -> Self {
        let mut by_leaf: HashMap<String, Vec<CodeType>> = HashMap::new();
        let mut by_title = HashMap::new();
        for typ in types {
            by_leaf
                .entry(leaf_name(&typ.title).to_string())
                .or_default()
                .push(typ.clone());
            by_title.insert(typ.title.clone(), typ);
        }
        Self { by_leaf, by_title }
    }

    fn resolve(&self, raw: &str) -> Option<&CodeType> {
        let cleaned = clean_type(raw);
        if let Some(exact) = self.by_title.get(&cleaned) {
            return Some(exact);
        }
        let leaf = leaf_name(&cleaned);
        let matches = self.by_leaf.get(leaf)?;
        if matches.len() == 1 {
            return matches.first();
        }
        matches.iter().find(|typ| typ.title.ends_with(&cleaned))
    }
}

async fn load_code_functions(pool: &PgPool, corpus_slug: &str) -> Result<Vec<CodeFn>> {
    let rows = sqlx::query(
        r#"WITH RECURSIVE down(file_path, id) AS (
               SELECT file.path, e.dst_id
                 FROM brain_vault_nodes file
                 JOIN brain_vault_edges e
                   ON e.src_id = file.id
                  AND e.edge_type = 'contains'
                WHERE file.project = $1
                  AND file.node_type = 'content:file'
               UNION
               SELECT down.file_path, e.dst_id
                 FROM down
                 JOIN brain_vault_edges e
                   ON e.src_id = down.id
                  AND e.edge_type = 'contains'
           )
           SELECT f.path,
                  down.file_path,
                  f.start_line,
                  f.end_line
             FROM down
             JOIN brain_vault_nodes f ON f.id = down.id
            WHERE f.project = $1
              AND f.node_type = 'code:function'
              AND f.valid_until IS NULL
              AND f.start_line IS NOT NULL
              AND f.end_line IS NOT NULL"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CodeFn {
            path: r.get("path"),
            file_path: r.get("file_path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
        })
        .collect())
}

async fn load_code_types(pool: &PgPool, corpus_slug: &str) -> Result<Vec<CodeType>> {
    let rows = sqlx::query(
        r#"SELECT path, title
             FROM brain_vault_nodes
            WHERE project = $1
              AND node_type IN ('code:struct', 'code:enum')
              AND valid_until IS NULL"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CodeType {
            path: r.get("path"),
            title: r.get("title"),
        })
        .collect())
}

fn parse_rust_file(source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    parser.parse(source, None)
}

fn walk_functions<F>(tree: &tree_sitter::Tree, bytes: &[u8], line_starts: &[usize], visit: &mut F)
where
    F: FnMut(&Node),
{
    fn walk<F>(node: &Node, bytes: &[u8], line_starts: &[usize], visit: &mut F)
    where
        F: FnMut(&Node),
    {
        if node.kind() == "function_item" {
            visit(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let _ = (bytes, line_starts);
            walk(&child, bytes, line_starts, visit);
        }
    }

    walk(&tree.root_node(), bytes, line_starts, visit);
}

fn resolve_function(functions: &[CodeFn], start_line: i32, end_line: i32) -> Option<&CodeFn> {
    functions
        .iter()
        .find(|f| f.start_line == start_line && f.end_line == end_line)
        .or_else(|| {
            functions
                .iter()
                .filter(|f| f.start_line <= start_line && end_line <= f.end_line)
                .min_by_key(|f| f.end_line - f.start_line)
        })
}

fn function_signature<'a>(node: &Node, bytes: &'a [u8]) -> &'a str {
    let end = node
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    std::str::from_utf8(&bytes[node.start_byte()..end]).unwrap_or("")
}

fn request_dtos(signature: &str, line_starts: &[usize], base_offset: usize) -> Vec<ApiSite> {
    let params = signature
        .find("->")
        .map(|arrow| &signature[..arrow])
        .unwrap_or(signature);
    ["Json", "Query", "Form", "Path"]
        .into_iter()
        .flat_map(|extractor| generic_args(params, extractor, line_starts, base_offset))
        .collect()
}

fn response_dtos(signature: &str, line_starts: &[usize], base_offset: usize) -> Vec<ApiSite> {
    let Some(arrow) = signature.find("->") else {
        return Vec::new();
    };
    generic_args(
        &signature[arrow..],
        "Json",
        line_starts,
        base_offset + arrow,
    )
}

fn generic_args(
    text: &str,
    token: &'static str,
    line_starts: &[usize],
    base_offset: usize,
) -> Vec<ApiSite> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while let Some(pos) = text[offset..].find(token) {
        let at = offset + pos;
        let before = text[..at].chars().next_back();
        let after_token = at + token.len();
        if before.is_some_and(is_ident_char) {
            offset = after_token;
            continue;
        }
        let rest = &text[after_token..];
        let ws = rest.len() - rest.trim_start().len();
        if !rest[ws..].starts_with('<') {
            offset = after_token;
            continue;
        }
        if let Some((arg, end)) = balanced_angle_arg(&rest[ws..]) {
            out.push(ApiSite {
                extractor: token,
                typ: first_type_arg(&arg),
                line: byte_to_line(line_starts, base_offset + after_token + ws),
            });
            offset = after_token + ws + end;
        } else {
            offset = after_token;
        }
    }
    out
}

fn balanced_angle_arg(text: &str) -> Option<(String, usize)> {
    let mut depth = 0i32;
    let mut start = None;
    for (idx, ch) in text.char_indices() {
        match ch {
            '<' => {
                depth += 1;
                if start.is_none() {
                    start = Some(idx + 1);
                }
            }
            '>' => {
                depth -= 1;
                if depth == 0 {
                    let s = start?;
                    return Some((text[s..idx].trim().to_string(), idx + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn first_type_arg(text: &str) -> String {
    let mut depth = 0i32;
    for (idx, ch) in text.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => return text[..idx].trim().to_string(),
            _ => {}
        }
    }
    text.trim().to_string()
}

fn external_calls(body: &str, line_starts: &[usize], base_offset: usize) -> Vec<ExternalCall> {
    let has_http_call = body.contains("reqwest::get")
        || body.contains("reqwest::post")
        || body.contains("reqwest::Client")
        || body.contains(".request(")
        || body.contains(".get(")
        || body.contains(".post(")
        || body.contains(".put(")
        || body.contains(".delete(")
        || body.contains(".patch(");
    if !has_http_call {
        return Vec::new();
    }

    let mut calls = Vec::new();
    for (url, at) in http_literals(body) {
        let service = host_from_url(&url).unwrap_or_else(|| "dynamic".to_string());
        calls.push(ExternalCall {
            service,
            url: Some(url),
            confidence: 0.9,
            line: byte_to_line(line_starts, base_offset + at),
        });
    }

    if calls.is_empty() {
        calls.push(ExternalCall {
            service: "dynamic".to_string(),
            url: None,
            confidence: 0.6,
            line: byte_to_line(line_starts, base_offset),
        });
    }

    calls
}

fn http_literals(body: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for needle in ["http://", "https://"] {
        let mut offset = 0usize;
        while let Some(pos) = body[offset..].find(needle) {
            let at = offset + pos;
            let tail = &body[at..];
            let end = tail
                .find(|ch: char| ch == '"' || ch == '\'' || ch.is_whitespace() || ch == ')')
                .unwrap_or(tail.len());
            out.push((tail[..end].to_string(), at));
            offset = at + end;
        }
    }
    out.sort_by_key(|(_, at)| *at);
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn host_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches(':');
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn clean_type(raw: &str) -> String {
    let mut s = raw.trim();
    while let Some(rest) = s.strip_prefix('&') {
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_prefix("mut ") {
        s = rest.trim_start();
    }
    s.trim_matches(|ch| matches!(ch, '(' | ')' | '[' | ']'))
        .trim()
        .to_string()
}

fn leaf_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn node_text<'a>(node: &Node, bytes: &'a [u8]) -> &'a str {
    std::str::from_utf8(&bytes[node.start_byte()..node.end_byte()]).unwrap_or("")
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

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
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
        collect_files_inner(&entry?.path(), out)?;
    }
    Ok(())
}

fn service_path(corpus: &str, service: &str) -> String {
    format!("ext://{corpus}/{service}")
}

pub async fn api(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<ApiHandlerSummary>> {
    let rows = sqlx::query(
        r#"SELECT f.title AS handler,
                  f.project AS corpus,
                  e.edge_type,
                  t.title AS dto
             FROM brain_vault_edges e
             JOIN brain_vault_nodes f ON f.id = e.src_id
             JOIN brain_vault_nodes t ON t.id = e.dst_id
            WHERE e.edge_type IN ('accepts', 'returns')
              AND f.node_type = 'code:function'
              AND t.node_type IN ('code:struct', 'code:enum')
              AND ($1::text IS NULL OR f.project = $1)
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = f.project), 0)
              )
              AND COALESCE(t.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = t.project), 0)
              )
            ORDER BY f.project, f.title, e.edge_type, t.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let mut by_handler: BTreeMap<(String, String), ApiHandlerSummary> = BTreeMap::new();
    for row in rows {
        let handler: String = row.get("handler");
        let corpus: String = row.get("corpus");
        let edge_type: String = row.get("edge_type");
        let dto: String = row.get("dto");
        let entry = by_handler
            .entry((corpus.clone(), handler.clone()))
            .or_insert_with(|| ApiHandlerSummary {
                handler,
                corpus,
                accepts: Vec::new(),
                returns: Vec::new(),
            });
        match edge_type.as_str() {
            "accepts" => push_unique(&mut entry.accepts, dto),
            "returns" => push_unique(&mut entry.returns, dto),
            _ => {}
        }
    }

    Ok(by_handler.into_values().collect())
}

pub async fn external(
    pool: &PgPool,
    corpus_slug: Option<&str>,
) -> Result<Vec<ExternalServiceSummary>> {
    let rows = sqlx::query(
        r#"SELECT s.title AS service,
                  s.project AS corpus,
                  f.title AS caller
             FROM brain_vault_nodes s
             JOIN brain_vault_edges e
               ON e.dst_id = s.id
              AND e.edge_type = 'calls_external'
             JOIN brain_vault_nodes f ON f.id = e.src_id
            WHERE s.node_type = 'ext:service'
              AND f.node_type = 'code:function'
              AND ($1::text IS NULL OR s.project = $1)
              AND COALESCE(s.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = s.project), 0)
              )
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = s.project), 0)
              )
            ORDER BY s.project, s.title, f.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let mut by_service: BTreeMap<(String, String), ExternalServiceSummary> = BTreeMap::new();
    for row in rows {
        let service: String = row.get("service");
        let corpus: String = row.get("corpus");
        let caller: String = row.get("caller");
        let entry = by_service
            .entry((corpus.clone(), service.clone()))
            .or_insert_with(|| ExternalServiceSummary {
                service,
                corpus,
                callers: Vec::new(),
            });
        push_unique(&mut entry.callers, caller);
    }

    Ok(by_service.into_values().collect())
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}
