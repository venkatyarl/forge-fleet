use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct DbSchemaExtractor;

#[derive(Debug, Clone, Default)]
struct Column {
    table: String,
    name: String,
    type_text: String,
    nullable: bool,
    default: Option<String>,
    check: Option<String>,
}

#[derive(Debug, Clone)]
enum Change {
    CreatesTable { table: String },
    CreatesColumn { column: Column },
    AltersTable { table: String },
    AltersColumn { column: Column },
    DropsTable { table: String },
    DropsColumn { table: String, column: String },
}

#[derive(Debug, Clone)]
struct Migration {
    version: u32,
    name: String,
    sql_const: String,
}

#[async_trait::async_trait]
impl Extractor for DbSchemaExtractor {
    fn name(&self) -> &'static str {
        "db_schema"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root, "sql")? {
                if let Ok(sql) = fs::read_to_string(&path) {
                    let changes = parse_sql_changes(&sql);
                    facts.add_schema_changes(changes, "sql_parse", 0.9, None);
                }
            }
        }

        let mut rust_strings = Vec::new();
        let mut const_sql: HashMap<String, String> = HashMap::new();
        let mut migrations = Vec::new();
        for root in &ctx.roots {
            for path in collect_files(root, "rs")? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                for lit in rust_string_literals(&source) {
                    if contains_ddl(&lit) {
                        rust_strings.push(lit);
                    }
                }
                const_sql.extend(extract_schema_consts(&source));
                migrations.extend(extract_pg_migrations(&source));
            }
        }

        for sql in rust_strings {
            let changes = parse_sql_changes(&sql);
            facts.add_schema_changes(changes, "rust_embedded_ddl", 0.85, None);
        }

        for migration in migrations {
            if let Some(sql) = const_sql.get(&migration.sql_const) {
                let changes = parse_sql_changes(sql);
                facts.add_migration(&migration);
                facts.add_schema_changes(changes, "rust_embedded_ddl", 0.9, Some(&migration));
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    tables: BTreeSet<String>,
    columns: BTreeMap<(String, String), Column>,
    migrations: BTreeMap<u32, String>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            tables: BTreeSet::new(),
            columns: BTreeMap::new(),
            migrations: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_migration(&mut self, migration: &Migration) {
        self.migrations
            .insert(migration.version, migration.name.clone());
    }

    fn add_schema_changes(
        &mut self,
        changes: Vec<Change>,
        provenance: &str,
        confidence: f32,
        migration: Option<&Migration>,
    ) {
        for change in changes {
            match change {
                Change::CreatesTable { table } => {
                    self.add_table(&table);
                    if let Some(m) = migration {
                        self.add_lineage_edge(
                            m,
                            &table_path(self.corpus, &table),
                            "creates",
                            provenance,
                            confidence,
                        );
                    }
                }
                Change::CreatesColumn { column } => {
                    self.add_column(column.clone(), provenance, confidence);
                    if let Some(m) = migration {
                        self.add_lineage_edge(
                            m,
                            &column_path(self.corpus, &column.table, &column.name),
                            "creates",
                            provenance,
                            confidence,
                        );
                    }
                }
                Change::AltersTable { table } => {
                    self.add_table(&table);
                    if let Some(m) = migration {
                        self.add_lineage_edge(
                            m,
                            &table_path(self.corpus, &table),
                            "alters",
                            provenance,
                            confidence,
                        );
                    }
                }
                Change::AltersColumn { column } => {
                    self.add_column(column.clone(), provenance, confidence);
                    if let Some(m) = migration {
                        self.add_lineage_edge(
                            m,
                            &column_path(self.corpus, &column.table, &column.name),
                            "alters",
                            provenance,
                            confidence,
                        );
                    }
                }
                Change::DropsTable { table } => {
                    self.add_table(&table);
                    if let Some(m) = migration {
                        self.add_lineage_edge(
                            m,
                            &table_path(self.corpus, &table),
                            "drops",
                            provenance,
                            confidence,
                        );
                    }
                }
                Change::DropsColumn { table, column } => {
                    self.add_table(&table);
                    let col = Column {
                        table: table.clone(),
                        name: column.clone(),
                        nullable: true,
                        ..Column::default()
                    };
                    self.add_column(col, provenance, confidence);
                    if let Some(m) = migration {
                        self.add_lineage_edge(
                            m,
                            &column_path(self.corpus, &table, &column),
                            "drops",
                            provenance,
                            confidence,
                        );
                    }
                }
            }
        }
    }

    fn add_table(&mut self, table: &str) {
        self.tables.insert(table.to_string());
    }

    fn add_column(&mut self, column: Column, provenance: &str, confidence: f32) {
        self.add_table(&column.table);
        let key = (column.table.clone(), column.name.clone());
        self.columns
            .entry(key)
            .and_modify(|existing| {
                if existing.type_text.is_empty() && !column.type_text.is_empty() {
                    existing.type_text = column.type_text.clone();
                }
                existing.nullable = existing.nullable && column.nullable;
                existing.default = existing.default.clone().or_else(|| column.default.clone());
                existing.check = existing.check.clone().or_else(|| column.check.clone());
            })
            .or_insert_with(|| column.clone());

        let evidence = json!({
            "type": column.type_text,
            "nullable": column.nullable,
            "default": column.default,
            "check": column.check,
        });
        self.add_edge(
            table_path(self.corpus, &column.table),
            column_path(self.corpus, &column.table, &column.name),
            "has_column",
            provenance,
            confidence,
            Some(evidence),
        );
    }

    fn add_lineage_edge(
        &mut self,
        migration: &Migration,
        dst_path: &str,
        edge_type: &'static str,
        provenance: &str,
        confidence: f32,
    ) {
        self.add_edge(
            migration_path(self.corpus, migration.version),
            dst_path.to_string(),
            edge_type,
            provenance,
            confidence,
            Some(json!({
                "version": migration.version,
                "name": migration.name,
                "sql_const": migration.sql_const,
            })),
        );
    }

    fn add_edge(
        &mut self,
        src_path: String,
        dst_path: String,
        edge_type: &'static str,
        provenance: &str,
        confidence: f32,
        evidence: Option<serde_json::Value>,
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
            provenance: provenance.to_string(),
            method: Some("EXTRACTED".to_string()),
            evidence,
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for table in &self.tables {
            out.push(Fact::Node {
                path: table_path(self.corpus, table),
                title: table.clone(),
                node_type: "db:table".to_string(),
                start_line: None,
                end_line: None,
                confidence: 0.9,
                provenance: "db_schema".to_string(),
            });
        }
        for column in self.columns.values() {
            out.push(Fact::Node {
                path: column_path(self.corpus, &column.table, &column.name),
                title: format!("{}.{}", column.table, column.name),
                node_type: "db:column".to_string(),
                start_line: None,
                end_line: None,
                confidence: 0.9,
                provenance: "db_schema".to_string(),
            });
        }
        for (version, name) in &self.migrations {
            out.push(Fact::Node {
                path: migration_path(self.corpus, *version),
                title: format!("V{version}_{name}"),
                node_type: "db:migration".to_string(),
                start_line: None,
                end_line: None,
                confidence: 0.9,
                provenance: "rust_embedded_ddl".to_string(),
            });
        }
        out.extend(self.edges);
        out
    }
}

fn collect_files(root: &Path, ext: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, ext, &mut out)?;
    out.sort_by_key(|path| {
        let s = path.to_string_lossy().to_lowercase();
        let priority = if s.contains("migration")
            || s.contains("/db/")
            || s.contains("/sql/")
            || s.contains("schema")
        {
            0
        } else {
            1
        };
        (priority, s)
    });
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

fn parse_sql_changes(sql: &str) -> Vec<Change> {
    let sql = strip_sql_comments(sql);
    let mut out = Vec::new();
    out.extend(parse_create_tables(&sql));
    out.extend(parse_alter_tables(&sql));
    out.extend(parse_drop_tables(&sql));
    out
}

fn parse_create_tables(sql: &str) -> Vec<Change> {
    let re = Regex::new(r#"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([^\s(]+)\s*\("#)
        .expect("valid create table regex");
    let mut out = Vec::new();
    for caps in re.captures_iter(sql) {
        let Some(table) = caps.get(1).map(|m| normalize_ident(m.as_str())) else {
            continue;
        };
        let Some(open_pos) = caps.get(0).map(|m| m.end() - 1) else {
            continue;
        };
        let Some(close_pos) = matching_paren(sql, open_pos) else {
            continue;
        };
        let body = &sql[open_pos + 1..close_pos];
        out.push(Change::CreatesTable {
            table: table.clone(),
        });
        for line in split_top_level_commas(body) {
            if let Some(column) = parse_column_def(&table, &line) {
                out.push(Change::CreatesColumn { column });
            }
        }
    }
    out
}

fn parse_alter_tables(sql: &str) -> Vec<Change> {
    let add_re = Regex::new(
        r#"(?is)\bALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?([^\s;]+)\s+ADD\s+(?:COLUMN\s+)?(?:IF\s+NOT\s+EXISTS\s+)?(.+?);"#,
    )
    .expect("valid alter table add regex");
    let drop_col_re = Regex::new(
        r#"(?is)\bALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?([^\s;]+)\s+DROP\s+(?:COLUMN\s+)?(?:IF\s+EXISTS\s+)?([^\s;,]+)"#,
    )
    .expect("valid alter table drop column regex");
    let alter_table_re = Regex::new(r#"(?is)\bALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?([^\s;]+)"#)
        .expect("valid alter table regex");
    let mut out = Vec::new();
    for caps in alter_table_re.captures_iter(sql) {
        if let Some(table) = caps.get(1).map(|m| normalize_ident(m.as_str())) {
            out.push(Change::AltersTable { table });
        }
    }
    for caps in add_re.captures_iter(sql) {
        let Some(table) = caps.get(1).map(|m| normalize_ident(m.as_str())) else {
            continue;
        };
        let Some(def) = caps.get(2).map(|m| m.as_str().trim()) else {
            continue;
        };
        if let Some(column) = parse_column_def(&table, def) {
            out.push(Change::AltersColumn { column });
        }
    }
    for caps in drop_col_re.captures_iter(sql) {
        let Some(table) = caps.get(1).map(|m| normalize_ident(m.as_str())) else {
            continue;
        };
        let Some(column) = caps.get(2).map(|m| normalize_ident(m.as_str())) else {
            continue;
        };
        out.push(Change::DropsColumn { table, column });
    }
    out
}

fn parse_drop_tables(sql: &str) -> Vec<Change> {
    let re = Regex::new(r#"(?is)\bDROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?([^\s;,]+)"#)
        .expect("valid drop table regex");
    re.captures_iter(sql)
        .filter_map(|caps| {
            caps.get(1).map(|m| Change::DropsTable {
                table: normalize_ident(m.as_str()),
            })
        })
        .collect()
}

fn parse_column_def(table: &str, raw: &str) -> Option<Column> {
    let def = raw.trim().trim_end_matches(',').trim();
    if def.is_empty() || is_table_constraint(def) {
        return None;
    }
    let (name_raw, rest) = split_first_token(def)?;
    let name = normalize_ident(name_raw);
    if name.is_empty() {
        return None;
    }
    let rest = rest.trim();
    let rest_upper = rest.to_uppercase();
    let nullable = !rest_upper.contains("NOT NULL");
    let default = capture_clause(rest, "DEFAULT");
    let check = capture_check(rest);
    let type_text = extract_type_text(rest);
    Some(Column {
        table: table.to_string(),
        name,
        type_text,
        nullable,
        default,
        check,
    })
}

fn is_table_constraint(def: &str) -> bool {
    let first = def
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches('"')
        .to_uppercase();
    // Only true table-level constraint line-starters. Deliberately NOT "KEY"/"INDEX":
    // `key` and `index` are common COLUMN names, and a bare leading KEY/INDEX is not a
    // Postgres table-constraint line (indexes are separate CREATE INDEX statements).
    matches!(
        first.as_str(),
        "PRIMARY" | "FOREIGN" | "CONSTRAINT" | "UNIQUE" | "CHECK" | "EXCLUDE"
    )
}

fn split_first_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if let Some(stripped) = s.strip_prefix('"') {
        let end = stripped.find('"')?;
        let ident = &s[..end + 2];
        return Some((ident, &s[end + 2..]));
    }
    let idx = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((&s[..idx], &s[idx..]))
}

fn extract_type_text(rest: &str) -> String {
    let upper = rest.to_uppercase();
    let mut end = rest.len();
    for kw in [
        " NOT NULL",
        " NULL",
        " DEFAULT",
        " CHECK",
        " PRIMARY",
        " REFERENCES",
        " UNIQUE",
        " CONSTRAINT",
        " COLLATE",
    ] {
        if let Some(pos) = upper.find(kw) {
            end = end.min(pos);
        }
    }
    rest[..end].trim().to_string()
}

fn capture_clause(rest: &str, keyword: &str) -> Option<String> {
    let upper = rest.to_uppercase();
    let start = upper.find(keyword)? + keyword.len();
    let tail = rest[start..].trim_start();
    let tail_upper = tail.to_uppercase();
    let mut end = tail.len();
    for kw in [
        " NOT NULL",
        " NULL",
        " CHECK",
        " PRIMARY",
        " REFERENCES",
        " UNIQUE",
        " CONSTRAINT",
    ] {
        if let Some(pos) = tail_upper.find(kw) {
            end = end.min(pos);
        }
    }
    Some(tail[..end].trim().trim_end_matches(',').to_string()).filter(|s| !s.is_empty())
}

fn capture_check(rest: &str) -> Option<String> {
    let upper = rest.to_uppercase();
    let pos = upper.find("CHECK")?;
    let open = rest[pos..].find('(')? + pos;
    let close = matching_paren(rest, open)?;
    Some(rest[open..=close].trim().to_string())
}

fn split_top_level_commas(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (idx, ch) in body.char_indices() {
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
                parts.push(body[start..idx].trim().to_string());
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < body.len() {
        parts.push(body[start..].trim().to_string());
    }
    parts
}

fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (idx, ch) in s.char_indices().filter(|(idx, _)| *idx >= open) {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_sql_comments(sql: &str) -> String {
    let line_re = Regex::new(r#"(?m)--.*$"#).expect("valid line comment regex");
    let block_re = Regex::new(r#"(?is)/\*.*?\*/"#).expect("valid block comment regex");
    let without_blocks = block_re.replace_all(sql, " ");
    line_re.replace_all(&without_blocks, " ").to_string()
}

fn normalize_ident(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(';')
        .split('.')
        .next_back()
        .unwrap_or(raw)
        .trim_matches('"')
        .trim_matches('`')
        .trim_matches('[')
        .trim_matches(']')
        .to_lowercase()
}

fn contains_ddl(s: &str) -> bool {
    let upper = s.to_uppercase();
    upper.contains("CREATE TABLE") || upper.contains("ALTER TABLE") || upper.contains("DROP TABLE")
}

fn rust_string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'r' {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < bytes.len() && bytes[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let body_start = j + 1;
                let mut k = body_start;
                while k < bytes.len() {
                    let hashes_match = if hashes == 0 {
                        true
                    } else {
                        k + hashes < bytes.len()
                            && bytes[k + 1..=k + hashes].iter().all(|b| *b == b'#')
                    };
                    if bytes[k] == b'"' && hashes_match {
                        out.push(source[body_start..k].to_string());
                        i = k + hashes + 1;
                        break;
                    }
                    k += 1;
                }
            }
        } else if bytes[i] == b'"' {
            let body_start = i + 1;
            let mut k = body_start;
            let mut escaped = false;
            let mut body = String::new();
            while k < bytes.len() {
                let ch = bytes[k] as char;
                if escaped {
                    body.push(match ch {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '"' => '"',
                        '\\' => '\\',
                        _ => ch,
                    });
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    out.push(body);
                    i = k + 1;
                    break;
                } else {
                    body.push(ch);
                }
                k += 1;
            }
            if i < body_start {
                i = k.saturating_add(1);
            }
        }
        i += 1;
    }
    out
}

fn extract_schema_consts(source: &str) -> HashMap<String, String> {
    let re = Regex::new(
        r##"(?s)pub\s+const\s+(SCHEMA_V[A-Z0-9_]+)\s*:\s*&str\s*=\s*(r#*".*?"#*|".*?")\s*;"##,
    )
    .expect("valid schema const regex");
    let mut out = HashMap::new();
    for caps in re.captures_iter(source) {
        let Some(name) = caps.get(1).map(|m| m.as_str().to_string()) else {
            continue;
        };
        let Some(lit) = caps.get(2).map(|m| m.as_str()) else {
            continue;
        };
        if let Some(value) = parse_rust_string_literal(lit) {
            out.insert(name, value);
        }
    }
    out
}

fn extract_pg_migrations(source: &str) -> Vec<Migration> {
    let re = Regex::new(
        r##"(?s)PgMigration\s*\{.*?version\s*:\s*(\d+).*?name\s*:\s*"([^"]+)".*?sql\s*:\s*schema::(SCHEMA_V[A-Z0-9_]+).*?\}"##,
    )
    .expect("valid pg migration regex");
    re.captures_iter(source)
        .filter_map(|caps| {
            Some(Migration {
                version: caps.get(1)?.as_str().parse().ok()?,
                name: caps.get(2)?.as_str().to_string(),
                sql_const: caps.get(3)?.as_str().to_string(),
            })
        })
        .collect()
}

fn parse_rust_string_literal(lit: &str) -> Option<String> {
    let lit = lit.trim();
    if let Some(rest) = lit.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        let after_hashes = &rest[hashes..];
        let body = after_hashes.strip_prefix('"')?;
        let suffix = format!("\"{}", "#".repeat(hashes));
        return body.strip_suffix(&suffix).map(ToString::to_string);
    }
    let body = lit.strip_prefix('"')?.strip_suffix('"')?;
    Some(
        body.replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\"),
    )
}

fn table_path(corpus: &str, table: &str) -> String {
    format!("db://{corpus}/{table}")
}

fn column_path(corpus: &str, table: &str, column: &str) -> String {
    format!("db://{corpus}/{table}.{column}")
}

fn migration_path(corpus: &str, version: u32) -> String {
    format!("db://{corpus}/migration/{version}")
}
