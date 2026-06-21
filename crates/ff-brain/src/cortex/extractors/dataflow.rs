use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

pub struct DataflowExtractor;

#[derive(Debug, Clone)]
struct CodeFn {
    path: String,
    file_path: String,
    start_line: i32,
    end_line: i32,
}

#[derive(Debug, Clone)]
struct DbNode {
    path: String,
    title: String,
    node_type: String,
}

#[derive(Debug, Clone)]
struct DbCatalog {
    tables: HashMap<String, String>,
    columns: HashMap<(String, String), String>,
    columns_by_name: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
enum SqlKind {
    Macro,
    Literal,
    Dynamic,
}

#[derive(Debug, Clone)]
struct SqlSite {
    sql: String,
    kind: SqlKind,
    line: i32,
}

#[derive(Debug, Clone)]
struct DataEdge {
    dst_path: String,
    edge_type: &'static str,
    confidence: f32,
    method: &'static str,
    line: i32,
    sql: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DataflowAccess {
    pub function: String,
    pub path: String,
    pub corpus: String,
    pub confidence: f32,
    pub method: Option<String>,
}

#[async_trait::async_trait]
impl Extractor for DataflowExtractor {
    fn name(&self) -> &'static str {
        "dataflow"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let code_fns = load_code_functions(ctx.pool, ctx.corpus_slug).await?;
        let catalog = DbCatalog::load(ctx.pool, ctx.corpus_slug).await?;
        if code_fns.is_empty() || (catalog.tables.is_empty() && catalog.columns.is_empty()) {
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

        let mut facts = FactBuilder::new();
        for root in &ctx.roots {
            for path in collect_files(root)? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let file_path = path.to_string_lossy().to_string();
                let Some(file_fns) = functions_by_file.get(&file_path) else {
                    continue;
                };
                let Some(tree) = parse_rust_file(&source) else {
                    continue;
                };
                let line_starts = line_start_offsets(&source);
                walk_functions(&tree, &mut |node| {
                    let start_line = byte_to_line(&line_starts, node.start_byte());
                    let end_line = byte_to_line(&line_starts, node.end_byte());
                    let Some(function) = resolve_function(file_fns, start_line, end_line) else {
                        return;
                    };
                    let body = node
                        .child_by_field_name("body")
                        .map(|body| node_text(&body, source.as_bytes()))
                        .unwrap_or_default();
                    for site in sql_sites(body, &line_starts, node.start_byte()) {
                        for edge in parse_sql_site(&site, &catalog) {
                            facts.add_edge(function.path.clone(), edge);
                        }
                    }
                });
            }
        }

        Ok(facts.finish())
    }
}

impl DbCatalog {
    async fn load(pool: &PgPool, corpus_slug: &str) -> Result<Self> {
        // Deliberately no current_generation filter: db_schema writes these nodes earlier
        // in this same reindex pass, before the generation is published.
        let rows = sqlx::query(
            r#"SELECT path, title, node_type
                 FROM brain_vault_nodes
                WHERE project = $1
                  AND node_type IN ('db:table', 'db:column')
                ORDER BY node_type, title"#,
        )
        .bind(corpus_slug)
        .fetch_all(pool)
        .await?;

        let mut tables = HashMap::new();
        let mut columns = HashMap::new();
        let mut columns_by_name: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let node = DbNode {
                path: row.get("path"),
                title: row.get("title"),
                node_type: row.get("node_type"),
            };
            match node.node_type.as_str() {
                "db:table" => {
                    tables.insert(norm_ident(&node.title), node.path);
                }
                "db:column" => {
                    if let Some((table, column)) = node.title.split_once('.') {
                        let table = norm_ident(table);
                        let column = norm_ident(column);
                        columns.insert((table.clone(), column.clone()), node.path);
                        columns_by_name.entry(column).or_default().push(table);
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            tables,
            columns,
            columns_by_name,
        })
    }

    fn table_path(&self, table: &str) -> Option<String> {
        self.tables.get(&norm_ident(table)).cloned()
    }

    fn column_path(&self, table: &str, column: &str) -> Option<String> {
        self.columns
            .get(&(norm_ident(table), norm_ident(column)))
            .cloned()
    }

    fn resolve_column(&self, scopes: &BTreeMap<String, String>, raw: &str) -> Resolved {
        let raw = raw.trim();
        if let Some((qualifier, column)) = raw.split_once('.') {
            let qualifier = norm_ident(qualifier);
            let table = scopes.get(&qualifier).cloned().unwrap_or(qualifier);
            return self
                .column_path(&table, column)
                .map(Resolved::Column)
                .or_else(|| self.table_path(&table).map(Resolved::AmbiguousTable))
                .unwrap_or(Resolved::None);
        }

        let column = norm_ident(raw);
        let matches: Vec<String> = scopes
            .values()
            .filter(|table| {
                self.columns
                    .contains_key(&((*table).clone(), column.clone()))
            })
            .cloned()
            .collect();
        if matches.len() == 1 {
            return self
                .column_path(&matches[0], &column)
                .map(Resolved::Column)
                .unwrap_or(Resolved::None);
        }
        if scopes.len() == 1 {
            if let Some(table) = scopes.values().next() {
                if let Some(path) = self.column_path(table, &column) {
                    return Resolved::Column(path);
                }
            }
        }
        if matches.len() > 1 {
            return Resolved::AmbiguousTables(matches);
        }
        if let Some(tables) = self.columns_by_name.get(&column) {
            if tables.len() == 1 {
                return self
                    .column_path(&tables[0], &column)
                    .map(Resolved::Column)
                    .unwrap_or(Resolved::None);
            }
        }
        Resolved::None
    }
}

enum Resolved {
    Column(String),
    AmbiguousTable(String),
    AmbiguousTables(Vec<String>),
    None,
}

struct FactBuilder {
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl FactBuilder {
    fn new() -> Self {
        Self {
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_edge(&mut self, src_path: String, edge: DataEdge) {
        let key = (
            src_path.clone(),
            edge.dst_path.clone(),
            edge.edge_type.to_string(),
        );
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path: edge.dst_path,
            edge_type: edge.edge_type.to_string(),
            confidence: edge.confidence,
            provenance: "dataflow".to_string(),
            method: Some(edge.method.to_string()),
            evidence: Some(json!({
                "line": edge.line,
                "sql": truncate_sql(&edge.sql),
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        self.edges
    }
}

fn parse_sql_site(site: &SqlSite, catalog: &DbCatalog) -> Vec<DataEdge> {
    let confidence = match site.kind {
        SqlKind::Macro => 0.9,
        SqlKind::Literal => 0.6,
        SqlKind::Dynamic => 0.4,
    };
    let method = match site.kind {
        SqlKind::Macro => "EXTRACTED",
        SqlKind::Literal => "INFERRED",
        SqlKind::Dynamic => "DYNAMIC",
    };
    if matches!(site.kind, SqlKind::Dynamic) {
        return dynamic_table_edges(site, catalog, confidence, method);
    }

    let sql = strip_sql_comments(&site.sql);
    let mut out = Vec::new();
    out.extend(parse_select(&sql, site, catalog, confidence, method));
    out.extend(parse_insert(&sql, site, catalog, confidence, method));
    out.extend(parse_update(&sql, site, catalog, confidence, method));
    out
}

fn parse_select(
    sql: &str,
    site: &SqlSite,
    catalog: &DbCatalog,
    confidence: f32,
    method: &'static str,
) -> Vec<DataEdge> {
    let re = Regex::new(r#"(?is)\bSELECT\s+(.*?)\s+FROM\s+([A-Za-z_][\w."]*)(.*)"#)
        .expect("valid select regex");
    let Some(caps) = re.captures(sql) else {
        return Vec::new();
    };
    let select_list = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
    let first_table = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
    let tail = caps.get(3).map(|m| m.as_str()).unwrap_or_default();
    let scopes = table_scopes(first_table, tail);
    let mut out = Vec::new();

    for item in split_top_level_commas(select_list) {
        let expr = item.split_whitespace().next().unwrap_or("").trim();
        if expr == "*" || expr.ends_with(".*") {
            let table = expr.strip_suffix(".*").and_then(|q| {
                let q = norm_ident(q);
                scopes.get(&q).cloned().or(Some(q))
            });
            add_table_reads(
                &mut out,
                catalog,
                table.into_iter().collect(),
                site,
                confidence,
                method,
            );
            if expr == "*" {
                add_table_reads(
                    &mut out,
                    catalog,
                    scopes.values().cloned().collect(),
                    site,
                    confidence,
                    method,
                );
            }
            continue;
        }
        add_resolved_read(&mut out, catalog, &scopes, expr, site, confidence, method);
    }

    for raw in predicate_columns(sql) {
        add_resolved_read(&mut out, catalog, &scopes, &raw, site, confidence, method);
    }
    out
}

fn parse_insert(
    sql: &str,
    site: &SqlSite,
    catalog: &DbCatalog,
    confidence: f32,
    method: &'static str,
) -> Vec<DataEdge> {
    let re = Regex::new(r#"(?is)\bINSERT\s+INTO\s+([A-Za-z_][\w."]*)\s*(?:\((.*?)\))?"#)
        .expect("valid insert regex");
    let Some(caps) = re.captures(sql) else {
        return Vec::new();
    };
    let table = norm_ident(caps.get(1).map(|m| m.as_str()).unwrap_or_default());
    let mut out = Vec::new();
    if let Some(cols) = caps.get(2).map(|m| m.as_str()) {
        for col in split_top_level_commas(cols) {
            if let Some(path) = catalog.column_path(&table, &col) {
                out.push(edge(path, "writes", confidence, method, site));
            }
        }
    } else if let Some(path) = catalog.table_path(&table) {
        out.push(edge(path, "writes", confidence.min(0.4), "DYNAMIC", site));
    }
    out
}

fn parse_update(
    sql: &str,
    site: &SqlSite,
    catalog: &DbCatalog,
    confidence: f32,
    method: &'static str,
) -> Vec<DataEdge> {
    let re = Regex::new(r#"(?is)\bUPDATE\s+([A-Za-z_][\w."]*)\s+SET\s+(.*)"#)
        .expect("valid update regex");
    let Some(caps) = re.captures(sql) else {
        return Vec::new();
    };
    let table = norm_ident(caps.get(1).map(|m| m.as_str()).unwrap_or_default());
    let rest = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
    let set_part = split_keyword(rest, "WHERE").0;
    let mut out = Vec::new();
    for assign in split_top_level_commas(set_part) {
        let col = assign.split('=').next().unwrap_or("").trim();
        if let Some(path) = catalog.column_path(&table, col) {
            out.push(edge(path, "writes", confidence, method, site));
        }
    }
    let mut scopes = BTreeMap::new();
    scopes.insert(table.clone(), table);
    for raw in predicate_columns(sql) {
        add_resolved_read(&mut out, catalog, &scopes, &raw, site, confidence, method);
    }
    out
}

fn dynamic_table_edges(
    site: &SqlSite,
    catalog: &DbCatalog,
    confidence: f32,
    method: &'static str,
) -> Vec<DataEdge> {
    let lower = site.sql.to_ascii_lowercase();
    let writes = lower.contains("insert") || lower.contains("update") || lower.contains("delete");
    let edge_type = if writes { "writes" } else { "reads" };
    catalog
        .tables
        .iter()
        .filter(|(table, _)| contains_word(&lower, table))
        .map(|(_, path)| edge(path.clone(), edge_type, confidence, method, site))
        .collect()
}

fn add_resolved_read(
    out: &mut Vec<DataEdge>,
    catalog: &DbCatalog,
    scopes: &BTreeMap<String, String>,
    raw: &str,
    site: &SqlSite,
    confidence: f32,
    method: &'static str,
) {
    match catalog.resolve_column(scopes, raw) {
        Resolved::Column(path) => out.push(edge(path, "reads", confidence, method, site)),
        Resolved::AmbiguousTable(table) => add_table_reads(
            out,
            catalog,
            vec![table],
            site,
            confidence.min(0.6),
            "INFERRED",
        ),
        Resolved::AmbiguousTables(tables) => {
            add_table_reads(out, catalog, tables, site, confidence.min(0.6), "INFERRED")
        }
        Resolved::None => {}
    }
}

fn add_table_reads(
    out: &mut Vec<DataEdge>,
    catalog: &DbCatalog,
    tables: Vec<String>,
    site: &SqlSite,
    confidence: f32,
    method: &'static str,
) {
    for table in tables {
        if let Some(path) = catalog.table_path(&table) {
            out.push(edge(path, "reads", confidence, method, site));
        }
    }
}

fn edge(
    dst_path: String,
    edge_type: &'static str,
    confidence: f32,
    method: &'static str,
    site: &SqlSite,
) -> DataEdge {
    DataEdge {
        dst_path,
        edge_type,
        confidence,
        method,
        line: site.line,
        sql: site.sql.clone(),
    }
}

fn table_scopes(first_table: &str, tail: &str) -> BTreeMap<String, String> {
    let mut scopes = BTreeMap::new();
    add_table_scope(&mut scopes, first_table, tail);
    let join_re = Regex::new(r#"(?is)\bJOIN\s+([A-Za-z_][\w."]*)(?:\s+(?:AS\s+)?([A-Za-z_]\w*))?"#)
        .expect("valid join regex");
    for caps in join_re.captures_iter(tail) {
        let table = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let alias = caps.get(2).map(|m| m.as_str());
        add_named_scope(&mut scopes, table, alias);
    }
    scopes
}

fn add_table_scope(scopes: &mut BTreeMap<String, String>, table: &str, tail: &str) {
    let alias_re = Regex::new(r#"(?is)^\s*(?:AS\s+)?([A-Za-z_]\w*)"#).expect("valid alias regex");
    let alias = alias_re
        .captures(tail)
        .and_then(|caps| caps.get(1).map(|m| m.as_str()))
        .filter(|alias| !is_sql_keyword(alias));
    add_named_scope(scopes, table, alias);
}

fn add_named_scope(scopes: &mut BTreeMap<String, String>, table: &str, alias: Option<&str>) {
    let table = norm_ident(table);
    if table.is_empty() {
        return;
    }
    scopes.insert(table.clone(), table.clone());
    if let Some(alias) = alias {
        scopes.insert(norm_ident(alias), table);
    }
}

fn predicate_columns(sql: &str) -> Vec<String> {
    let re = Regex::new(
        r#"(?is)(?:\bWHERE\b|\bON\b|\bAND\b|\bOR\b)\s+([A-Za-z_][\w."]*)\s*(?:=|<>|!=|<=|>=|<|>|\bIN\b|\bLIKE\b|\bIS\b)"#,
    )
    .expect("valid predicate regex");
    re.captures_iter(sql)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .filter(|col| !is_sql_keyword(col))
        .collect()
}

fn sql_sites(body: &str, line_starts: &[usize], base_offset: usize) -> Vec<SqlSite> {
    let mut out = Vec::new();
    for (literal, at, is_macro) in sql_literals(body) {
        if looks_like_sql(&literal) {
            out.push(SqlSite {
                sql: literal,
                kind: if is_macro {
                    SqlKind::Macro
                } else {
                    SqlKind::Literal
                },
                line: byte_to_line(line_starts, base_offset + at),
            });
        }
    }
    for (snippet, at) in dynamic_sql_snippets(body) {
        out.push(SqlSite {
            sql: snippet,
            kind: SqlKind::Dynamic,
            line: byte_to_line(line_starts, base_offset + at),
        });
    }
    out
}

fn sql_literals(body: &str) -> Vec<(String, usize, bool)> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if let Some((value, end)) = cooked_string(body, i) {
                let mut pstart = i.saturating_sub(48);
                while pstart < i && !body.is_char_boundary(pstart) {
                    pstart += 1;
                }
                let prefix = &body[pstart..i];
                out.push((value, i, is_sqlx_macro_prefix(prefix)));
                i = end;
                continue;
            }
        }
        if bytes[i] == b'r' {
            if let Some((value, end)) = raw_string(body, i) {
                let mut pstart = i.saturating_sub(48);
                while pstart < i && !body.is_char_boundary(pstart) {
                    pstart += 1;
                }
                let prefix = &body[pstart..i];
                out.push((value, i, is_sqlx_macro_prefix(prefix)));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn cooked_string(s: &str, start: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut escaped = false;
    for (idx, ch) in s[start + 1..].char_indices() {
        let at = start + 1 + idx;
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some((out, at + 1));
        } else {
            out.push(ch);
        }
    }
    None
}

fn raw_string(s: &str, start: usize) -> Option<(String, usize)> {
    let rest = &s[start..];
    if !rest.starts_with('r') {
        return None;
    }
    let mut hashes = 0usize;
    let mut chars = rest.chars();
    if chars.next()? != 'r' {
        return None;
    }
    while matches!(chars.clone().next(), Some('#')) {
        hashes += 1;
        chars.next();
    }
    if chars.next()? != '"' {
        return None;
    }
    let content_start = start + 1 + hashes + 1;
    let terminator = format!("\"{}", "#".repeat(hashes));
    let tail = &s[content_start..];
    let end_rel = tail.find(&terminator)?;
    Some((
        tail[..end_rel].to_string(),
        content_start + end_rel + terminator.len(),
    ))
}

fn is_sqlx_macro_prefix(prefix: &str) -> bool {
    let compact = prefix.split_whitespace().collect::<String>();
    let scope_start = compact
        .rfind([';', '{', '}'])
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let scope = &compact[scope_start..];
    [
        "sqlx::query!(",
        "sqlx::query_as!(",
        "sqlx::query_scalar!(",
        "query!(",
        "query_as!(",
        "query_scalar!(",
    ]
    .iter()
    .any(|needle| scope.contains(needle))
}

fn dynamic_sql_snippets(body: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for needle in ["format!(", "concat!(", "sqlx::query(", "query("] {
        let mut offset = 0usize;
        while let Some(pos) = body[offset..].find(needle) {
            let at = offset + pos;
            // Snap the 500-byte window end DOWN to a char boundary — a raw
            // byte-slice panics when at+500 lands inside a multi-byte char
            // (e.g. '→' in a source comment).
            let mut end = body.len().min(at + 500);
            while end > at && !body.is_char_boundary(end) {
                end -= 1;
            }
            let snippet = body[at..end].to_string();
            if snippet.contains("SELECT")
                || snippet.contains("select")
                || snippet.contains("INSERT")
                || snippet.contains("insert")
                || snippet.contains("UPDATE")
                || snippet.contains("update")
            {
                out.push((snippet, at));
            }
            offset = at + needle.len();
        }
    }
    out
}

fn looks_like_sql(s: &str) -> bool {
    let trimmed = s.trim_start().to_ascii_lowercase();
    trimmed.starts_with("select ")
        || trimmed.starts_with("insert ")
        || trimmed.starts_with("update ")
        || trimmed.starts_with("delete ")
        || trimmed.contains(" from ")
        || trimmed.contains(" into ")
}

fn split_keyword<'a>(s: &'a str, keyword: &str) -> (&'a str, Option<&'a str>) {
    let pat = format!(r#"(?is)\b{}\b"#, regex::escape(keyword));
    let re = Regex::new(&pat).expect("valid keyword regex");
    if let Some(m) = re.find(s) {
        (&s[..m.start()], Some(&s[m.end()..]))
    } else {
        (s, None)
    }
}

fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (idx, ch) in s.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..idx].trim().to_string());
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(s[start..].trim().to_string());
    }
    out.into_iter().filter(|v| !v.is_empty()).collect()
}

fn strip_sql_comments(sql: &str) -> String {
    let line_re = Regex::new(r#"(?m)--.*$"#).expect("valid line comment regex");
    let block_re = Regex::new(r#"(?s)/\*.*?\*/"#).expect("valid block comment regex");
    block_re
        .replace_all(&line_re.replace_all(sql, ""), "")
        .to_string()
}

fn norm_ident(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .split('.')
        .next_back()
        .unwrap_or("")
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .to_ascii_lowercase()
}

fn is_sql_keyword(raw: &str) -> bool {
    matches!(
        norm_ident(raw).as_str(),
        "select"
            | "from"
            | "where"
            | "join"
            | "on"
            | "as"
            | "and"
            | "or"
            | "group"
            | "order"
            | "limit"
            | "returning"
            | "set"
            | "values"
            | "left"
            | "right"
            | "inner"
            | "outer"
            | "full"
    )
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    let re =
        Regex::new(&format!(r#"(?i)\b{}\b"#, regex::escape(needle))).expect("valid word regex");
    re.is_match(haystack)
}

fn truncate_sql(sql: &str) -> String {
    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() > 300 {
        // Snap to a char boundary — &compact[..300] panics when byte 300 lands
        // inside a multi-byte char (e.g. '✓' in a sqlx string literal).
        let mut end = 300;
        while end > 0 && !compact.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &compact[..end])
    } else {
        compact
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

fn parse_rust_file(source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    parser.parse(source, None)
}

fn walk_functions<F>(tree: &tree_sitter::Tree, visit: &mut F)
where
    F: FnMut(&Node),
{
    fn walk<F>(node: &Node, visit: &mut F)
    where
        F: FnMut(&Node),
    {
        if node.kind() == "function_item" {
            visit(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(&child, visit);
        }
    }

    walk(&tree.root_node(), visit);
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

pub async fn readers(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    column: &str,
) -> Result<Vec<DataflowAccess>> {
    accessors(pool, corpus_slug, column, "reads").await
}

pub async fn writers(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    column: &str,
) -> Result<Vec<DataflowAccess>> {
    accessors(pool, corpus_slug, column, "writes").await
}

async fn accessors(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    column: &str,
    edge_type: &str,
) -> Result<Vec<DataflowAccess>> {
    let rows = sqlx::query(
        r#"SELECT f.title AS function,
                  f.path,
                  f.project AS corpus,
                  e.confidence,
                  e.method
             FROM brain_vault_nodes c
             JOIN brain_vault_edges e ON e.dst_id = c.id
             JOIN brain_vault_nodes f ON f.id = e.src_id
            WHERE c.node_type = 'db:column'
              AND c.title = $1
              AND e.edge_type = $2
              AND f.node_type = 'code:function'
              AND ($3::text IS NULL OR c.project = $3)
              AND COALESCE(c.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
              AND f.valid_until IS NULL
            ORDER BY f.project, f.title"#,
    )
    .bind(column)
    .bind(edge_type)
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| DataflowAccess {
            function: row.get("function"),
            path: row.get("path"),
            corpus: row.get("corpus"),
            confidence: row.get("confidence"),
            method: row.get("method"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> DbCatalog {
        let mut tables = HashMap::new();
        tables.insert("work_items".to_string(), "db://test/work_items".to_string());
        tables.insert("projects".to_string(), "db://test/projects".to_string());
        let mut columns = HashMap::new();
        for (table, column) in [
            ("work_items", "id"),
            ("work_items", "project_id"),
            ("work_items", "title"),
            ("work_items", "status"),
            ("projects", "id"),
            ("projects", "name"),
        ] {
            columns.insert(
                (table.to_string(), column.to_string()),
                format!("db://test/{table}.{column}"),
            );
        }
        let mut columns_by_name: HashMap<String, Vec<String>> = HashMap::new();
        for (table, column) in columns.keys() {
            columns_by_name
                .entry(column.clone())
                .or_default()
                .push(table.clone());
        }
        DbCatalog {
            tables,
            columns,
            columns_by_name,
        }
    }

    fn literal(sql: &str) -> SqlSite {
        SqlSite {
            sql: sql.to_string(),
            kind: SqlKind::Macro,
            line: 7,
        }
    }

    #[test]
    fn select_emits_column_reads_for_select_and_predicates() {
        let edges = parse_sql_site(
            &literal(
                "SELECT w.title, p.name FROM work_items w JOIN projects p ON p.id = w.project_id WHERE w.status = $1",
            ),
            &catalog(),
        );
        let got: HashSet<_> = edges
            .iter()
            .map(|e| (e.edge_type, e.dst_path.as_str()))
            .collect();
        assert!(got.contains(&("reads", "db://test/work_items.title")));
        assert!(got.contains(&("reads", "db://test/projects.name")));
        assert!(got.contains(&("reads", "db://test/projects.id")));
        assert!(got.contains(&("reads", "db://test/work_items.status")));
    }

    #[test]
    fn insert_and_update_emit_writes() {
        let catalog = catalog();
        let insert = parse_sql_site(
            &literal("INSERT INTO work_items (project_id, title) VALUES ($1, $2)"),
            &catalog,
        );
        assert!(
            insert
                .iter()
                .any(|e| e.edge_type == "writes" && e.dst_path == "db://test/work_items.title")
        );

        let update = parse_sql_site(
            &literal("UPDATE work_items SET status = $1 WHERE id = $2"),
            &catalog,
        );
        assert!(
            update
                .iter()
                .any(|e| e.edge_type == "writes" && e.dst_path == "db://test/work_items.status")
        );
        assert!(
            update
                .iter()
                .any(|e| e.edge_type == "reads" && e.dst_path == "db://test/work_items.id")
        );
    }

    #[test]
    fn query_as_macro_literal_is_extracted_even_with_type_argument() {
        assert!(is_sqlx_macro_prefix("let row = sqlx::query_as!(Thing, "));
    }
}
