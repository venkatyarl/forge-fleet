use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::{Result, anyhow};
use regex::Regex;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

pub struct TypesExtractor;

#[derive(Debug, Clone)]
struct CodeSymbol {
    path: String,
    title: String,
}

#[derive(Debug, Clone)]
struct TypeField {
    owner: String,
    name: String,
    type_text: String,
}

#[derive(Debug, Clone)]
struct TypeEdge {
    src: String,
    dst: String,
    edge_type: &'static str,
    confidence: f32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImplRow {
    pub typ: String,
    pub trait_name: String,
    pub corpus: String,
    pub confidence: f32,
    pub provenance: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldRow {
    pub owner: String,
    pub field: String,
    pub field_type: String,
    pub corpus: String,
    pub confidence: f32,
    pub provenance: String,
}

#[async_trait::async_trait]
impl Extractor for TypesExtractor {
    fn name(&self) -> &'static str {
        "types"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let symbols = load_code_symbols(ctx.pool, ctx.corpus_slug).await?;
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let resolver = SymbolResolver::new(ctx.corpus_slug, symbols);
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root)? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let path_text = path.to_string_lossy();
                let parsed = match path.extension().and_then(|e| e.to_str()) {
                    Some("rs") => parse_rust_file(&path_text, &source),
                    Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs") => {
                        parse_typescript_file(&path_text, &source)
                    }
                    _ => None,
                };
                let Some(parsed) = parsed else {
                    continue;
                };
                for field in parsed.fields {
                    if let Some(owner) = resolver.resolve_type(&field.owner) {
                        facts.add_field(owner, field);
                    }
                }
                for edge in parsed.edges {
                    if let (Some(src), Some(dst)) = (
                        resolver.resolve_type(&edge.src),
                        resolver.resolve_type(&edge.dst),
                    ) {
                        facts.add_edge(src.path.clone(), dst.path.clone(), edge);
                    }
                }
            }
        }

        Ok(facts.finish())
    }
}

#[derive(Default)]
struct ParsedTypes {
    fields: Vec<TypeField>,
    edges: Vec<TypeEdge>,
}

struct FactBuilder<'a> {
    corpus: &'a str,
    fields: BTreeMap<(String, String), TypeField>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            fields: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_field(&mut self, owner: &CodeSymbol, field: TypeField) {
        let key = (owner.title.clone(), field.name.clone());
        self.fields.entry(key).or_insert_with(|| TypeField {
            owner: owner.title.clone(),
            name: field.name.clone(),
            type_text: field.type_text.clone(),
        });
        let dst_path = field_path(self.corpus, &owner.title, &field.name);
        self.add_fact_edge(
            owner.path.clone(),
            dst_path,
            "has_field",
            0.9,
            Some(json!({
                "type": field.type_text,
                "field": field.name,
                "owner": owner.title,
            })),
        );
    }

    fn add_edge(&mut self, src_path: String, dst_path: String, edge: TypeEdge) {
        self.add_fact_edge(src_path, dst_path, edge.edge_type, edge.confidence, None);
    }

    fn add_fact_edge(
        &mut self,
        src_path: String,
        dst_path: String,
        edge_type: &'static str,
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
            provenance: "ast".to_string(),
            method: Some("EXTRACTED".to_string()),
            evidence,
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for field in self.fields.values() {
            out.push(Fact::Node {
                path: field_path(self.corpus, &field.owner, &field.name),
                title: format!("{}.{}", field.owner, field.name),
                node_type: "type:field".to_string(),
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

struct SymbolResolver {
    corpus: String,
    by_title: HashMap<String, CodeSymbol>,
    by_leaf: HashMap<String, Vec<CodeSymbol>>,
}

impl SymbolResolver {
    fn new(corpus: &str, symbols: Vec<CodeSymbol>) -> Self {
        let mut by_title = HashMap::new();
        let mut by_leaf: HashMap<String, Vec<CodeSymbol>> = HashMap::new();
        for symbol in symbols {
            by_leaf
                .entry(leaf(&symbol.title).to_string())
                .or_default()
                .push(symbol.clone());
            by_title.insert(symbol.title.clone(), symbol);
        }
        Self {
            corpus: corpus.to_string(),
            by_title,
            by_leaf,
        }
    }

    fn resolve_type(&self, name: &str) -> Option<&CodeSymbol> {
        let normalized = normalize_type_name(name);
        if normalized.is_empty() {
            return None;
        }
        if let Some(symbol) = self.by_title.get(&normalized) {
            return Some(symbol);
        }
        let code_path = format!("code://{}/{}", self.corpus, normalized);
        if let Some(symbol) = self.by_title.values().find(|s| s.path == code_path) {
            return Some(symbol);
        }
        let short = leaf(&normalized);
        let candidates = self.by_leaf.get(short)?;
        if candidates.len() == 1 {
            return candidates.first();
        }
        candidates
            .iter()
            .find(|s| s.title.ends_with(&format!("::{normalized}")))
    }
}

async fn load_code_symbols(pool: &PgPool, corpus: &str) -> Result<Vec<CodeSymbol>> {
    // NOTE: deliberately NO current_generation filter here. This extractor reads
    // code:* nodes written EARLIER IN THE SAME reindex pass (at the in-progress
    // generation, before the atomic swap publishes it as current_generation). The
    // published-generation filter used by external readers would hide those
    // just-written nodes and yield 0 results. The per-corpus advisory lock
    // guarantees we only see this pass's nodes, so reading all is correct.
    let rows = sqlx::query(
        r#"SELECT path, title
             FROM brain_vault_nodes
            WHERE project = $1
              AND node_type IN ('code:struct', 'code:enum', 'code:trait', 'code:class', 'code:interface')
            ORDER BY title"#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CodeSymbol {
            path: row.get("path"),
            title: row.get("title"),
        })
        .collect())
}

fn parse_rust_file(file_path: &str, source: &str) -> Option<ParsedTypes> {
    let (_, module) = module_for_file(file_path);
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut parsed = ParsedTypes::default();
    walk_rust(tree.root_node(), source.as_bytes(), &module, &mut parsed);
    Some(parsed)
}

fn walk_rust(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    match node.kind() {
        "struct_item" => parse_rust_struct(node, bytes, module, parsed),
        "enum_item" => parse_rust_enum(node, bytes, module, parsed),
        "trait_item" => parse_rust_trait(node, bytes, module, parsed),
        "impl_item" => parse_rust_impl(node, bytes, module, parsed),
        "mod_item" => {
            let next_module = child_field_text(node, "name", bytes)
                .map(|name| join(module, &name))
                .unwrap_or_else(|| module.to_string());
            walk_children_rust(node, bytes, &next_module, parsed);
            return;
        }
        _ => {}
    }
    walk_children_rust(node, bytes, module, parsed);
}

fn walk_children_rust(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_rust(child, bytes, module, parsed);
    }
}

fn parse_rust_struct(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let Some(name) = child_field_text(node, "name", bytes) else {
        return;
    };
    let owner = join(module, &name);
    collect_rust_fields(node, bytes, &owner, None, parsed);
}

fn parse_rust_enum(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let Some(name) = child_field_text(node, "name", bytes) else {
        return;
    };
    let owner = join(module, &name);
    collect_rust_fields(node, bytes, &owner, None, parsed);
}

fn parse_rust_trait(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let Some(name) = child_field_text(node, "name", bytes) else {
        return;
    };
    let src = join(module, &name);
    if let Some(text) = node_text(node, bytes) {
        if let Some(header) = text.split('{').next() {
            if let Some((_, supers)) = header.split_once(':') {
                for supertrait in split_type_list(supers) {
                    parsed.edges.push(TypeEdge {
                        src: src.clone(),
                        dst: qualify_type(module, &supertrait),
                        edge_type: "extends",
                        confidence: 0.8,
                    });
                }
            }
        }
    }
}

fn parse_rust_impl(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let Some(type_name) = child_field_text(node, "type", bytes) else {
        return;
    };
    let Some(trait_name) = child_field_text(node, "trait", bytes) else {
        return;
    };
    parsed.edges.push(TypeEdge {
        src: qualify_type(module, &type_name),
        dst: qualify_type(module, &trait_name),
        edge_type: "implements",
        confidence: 0.9,
    });
}

fn collect_rust_fields(
    node: Node<'_>,
    bytes: &[u8],
    owner: &str,
    variant: Option<&str>,
    parsed: &mut ParsedTypes,
) {
    let mut tuple_index = 0usize;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "field_declaration" => {
                let Some(type_text) = child_field_text(child, "type", bytes) else {
                    continue;
                };
                let Some(name) = child_field_text(child, "name", bytes) else {
                    continue;
                };
                parsed.fields.push(TypeField {
                    owner: owner.to_string(),
                    name: qualify_field_name(variant, &name),
                    type_text,
                });
            }
            "ordered_field_declaration" => {
                let Some(type_text) = node_text(child, bytes) else {
                    continue;
                };
                parsed.fields.push(TypeField {
                    owner: owner.to_string(),
                    name: qualify_field_name(variant, &tuple_index.to_string()),
                    type_text: clean_type_text(&type_text),
                });
                tuple_index += 1;
            }
            "enum_variant" => {
                let name = child_field_text(child, "name", bytes);
                collect_rust_fields(child, bytes, owner, name.as_deref(), parsed);
            }
            _ => collect_rust_fields(child, bytes, owner, variant, parsed),
        }
    }
}

fn parse_typescript_file(file_path: &str, source: &str) -> Option<ParsedTypes> {
    let (_, module) = ts_module_for_file(file_path);
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let grammar = if matches!(ext, "ts" | "mts" | "cts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    } else {
        tree_sitter_typescript::LANGUAGE_TSX
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar.into()).ok()?;
    let tree = parser.parse(source, None)?;
    let mut parsed = ParsedTypes::default();
    walk_ts(tree.root_node(), source.as_bytes(), &module, &mut parsed);
    Some(parsed)
}

fn walk_ts(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" => {
            parse_ts_class(node, bytes, module, parsed)
        }
        "interface_declaration" => parse_ts_interface(node, bytes, module, parsed),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ts(child, bytes, module, parsed);
    }
}

fn parse_ts_class(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let Some(name) = child_field_text(node, "name", bytes) else {
        return;
    };
    let owner = join(module, &name);
    if let Some(text) = node_text(node, bytes) {
        if let Some(header) = text.split('{').next() {
            collect_ts_heritage(header, module, &owner, parsed);
        }
    }
    collect_ts_fields(node, bytes, &owner, parsed);
}

fn parse_ts_interface(node: Node<'_>, bytes: &[u8], module: &str, parsed: &mut ParsedTypes) {
    let Some(name) = child_field_text(node, "name", bytes) else {
        return;
    };
    let owner = join(module, &name);
    if let Some(text) = node_text(node, bytes) {
        if let Some(header) = text.split('{').next() {
            collect_ts_heritage(header, module, &owner, parsed);
        }
    }
    collect_ts_fields(node, bytes, &owner, parsed);
}

fn collect_ts_heritage(header: &str, module: &str, owner: &str, parsed: &mut ParsedTypes) {
    let extends = Regex::new(r"\bextends\s+([A-Za-z0-9_.$]+)").expect("valid TS extends regex");
    if let Some(caps) = extends.captures(header) {
        if let Some(target) = caps.get(1).map(|m| m.as_str()) {
            parsed.edges.push(TypeEdge {
                src: owner.to_string(),
                dst: qualify_type(module, target),
                edge_type: "extends",
                confidence: 0.8,
            });
        }
    }
    let implements =
        Regex::new(r"\bimplements\s+([A-Za-z0-9_.$,\s]+)").expect("valid TS implements regex");
    if let Some(caps) = implements.captures(header) {
        if let Some(targets) = caps.get(1).map(|m| m.as_str()) {
            for target in split_type_list(targets) {
                parsed.edges.push(TypeEdge {
                    src: owner.to_string(),
                    dst: qualify_type(module, &target),
                    edge_type: "implements",
                    confidence: 0.9,
                });
            }
        }
    }
}

fn collect_ts_fields(node: Node<'_>, bytes: &[u8], owner: &str, parsed: &mut ParsedTypes) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "public_field_definition"
            | "field_definition"
            | "property_signature"
            | "required_parameter" => {
                let Some(name) = child_field_text(child, "name", bytes) else {
                    continue;
                };
                let type_text = child
                    .child_by_field_name("type")
                    .and_then(|n| node_text(n, bytes))
                    .unwrap_or_default();
                parsed.fields.push(TypeField {
                    owner: owner.to_string(),
                    name,
                    type_text: clean_ts_type_text(&type_text),
                });
            }
            _ => collect_ts_fields(child, bytes, owner, parsed),
        }
    }
}

pub async fn impls(pool: &PgPool, corpus_slug: &str, trait_name: &str) -> Result<Vec<ImplRow>> {
    let rows = sqlx::query(
        r#"SELECT s.title AS typ,
                  t.title AS trait_name,
                  s.project AS corpus,
                  e.confidence,
                  e.provenance
             FROM brain_vault_edges e
             JOIN brain_vault_nodes s ON s.id = e.src_id
             JOIN brain_vault_nodes t ON t.id = e.dst_id
            WHERE e.edge_type = 'implements'
              AND s.project = $1
              AND t.project = $1
              AND (t.title = $2 OR t.title LIKE $3)
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = s.project), 0)
              )
            ORDER BY s.title"#,
    )
    .bind(corpus_slug)
    .bind(trait_name)
    .bind(format!("%::{trait_name}"))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ImplRow {
            typ: row.get("typ"),
            trait_name: row.get("trait_name"),
            corpus: row.get("corpus"),
            confidence: row.get("confidence"),
            provenance: row.get("provenance"),
        })
        .collect())
}

pub async fn fields(pool: &PgPool, corpus_slug: &str, type_name: &str) -> Result<Vec<FieldRow>> {
    let rows = sqlx::query(
        r#"SELECT owner.title AS owner,
                  field.title AS field,
                  owner.project AS corpus,
                  e.confidence,
                  e.provenance,
                  COALESCE(e.evidence->>'type', '') AS field_type
             FROM brain_vault_edges e
             JOIN brain_vault_nodes owner ON owner.id = e.src_id
             JOIN brain_vault_nodes field ON field.id = e.dst_id
            WHERE e.edge_type = 'has_field'
              AND field.node_type = 'type:field'
              AND owner.project = $1
              AND (owner.title = $2 OR owner.title LIKE $3)
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = owner.project), 0)
              )
            ORDER BY field.title"#,
    )
    .bind(corpus_slug)
    .bind(type_name)
    .bind(format!("%::{type_name}"))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| FieldRow {
            owner: row.get("owner"),
            field: row.get("field"),
            field_type: row.get("field_type"),
            corpus: row.get("corpus"),
            confidence: row.get("confidence"),
            provenance: row.get("provenance"),
        })
        .collect())
}

pub async fn handle_cli(pool: &PgPool, args: Vec<String>, default_corpus: String) -> Result<()> {
    let (verb, rest) = args.split_first().ok_or_else(|| {
        anyhow!("expected `ff cortex impls <trait>` or `ff cortex fields <type>`")
    })?;
    let (target, corpus, format) = parse_query_args(rest, default_corpus)?;
    match verb.as_str() {
        "impls" => print_impls(&impls(pool, &corpus, &target).await?, &target, &format),
        "fields" => print_fields(&fields(pool, &corpus, &target).await?, &target, &format),
        other => {
            return Err(anyhow!(
                "unknown cortex query '{other}' (expected `impls` or `fields`)"
            ));
        }
    }
    Ok(())
}

fn parse_query_args(args: &[String], default_corpus: String) -> Result<(String, String, String)> {
    let mut target = None;
    let mut corpus = default_corpus;
    let mut format = "table".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--corpus" => {
                i += 1;
                corpus = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow!("--corpus requires a value"))?;
            }
            "--format" => {
                i += 1;
                format = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow!("--format requires a value"))?;
            }
            value if value.starts_with("--") => return Err(anyhow!("unknown option '{value}'")),
            value => {
                if target.replace(value.to_string()).is_some() {
                    return Err(anyhow!("too many positional arguments"));
                }
            }
        }
        i += 1;
    }
    let target = target.ok_or_else(|| anyhow!("missing query target"))?;
    Ok((target, corpus, format))
}

fn print_impls(rows: &[ImplRow], trait_name: &str, format: &str) {
    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
        ),
        "names" => {
            for row in rows {
                println!("{}", row.typ);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no implementations of '{trait_name}' in cortex (run `ff cortex index`?)");
                return;
            }
            println!("cortex impls '{trait_name}' -- {} hit(s):", rows.len());
            for row in rows {
                println!(
                    "  {} -> {} ({}, confidence {:.2})",
                    row.typ, row.trait_name, row.corpus, row.confidence
                );
            }
        }
    }
}

fn print_fields(rows: &[FieldRow], type_name: &str, format: &str) {
    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
        ),
        "names" => {
            for row in rows {
                println!("{}", row.field);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no fields for '{type_name}' in cortex (run `ff cortex index`?)");
                return;
            }
            println!("cortex fields '{type_name}' -- {} hit(s):", rows.len());
            for row in rows {
                let typ = if row.field_type.is_empty() {
                    "-"
                } else {
                    &row.field_type
                };
                println!("  {}: {} ({})", row.field, typ, row.corpus);
            }
        }
    }
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rs" | "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
        ) {
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

fn field_path(corpus: &str, owner: &str, field: &str) -> String {
    format!("type://{corpus}/{owner}.{field}")
}

fn qualify_field_name(variant: Option<&str>, name: &str) -> String {
    variant
        .map(|v| format!("{v}.{name}"))
        .unwrap_or_else(|| name.to_string())
}

fn qualify_type(module: &str, text: &str) -> String {
    let normalized = normalize_type_name(text);
    if normalized.contains("::") {
        normalized
    } else if normalized.contains('.') {
        normalized.replace('.', "::")
    } else {
        join(module, &normalized)
    }
}

fn normalize_type_name(text: &str) -> String {
    let mut s = clean_type_text(text);
    if let Some(rest) = s.strip_prefix('&') {
        s = rest.trim().to_string();
    }
    while let Some(rest) = s.strip_prefix("mut ") {
        s = rest.trim().to_string();
    }
    if let Some(idx) = s.find('<') {
        s.truncate(idx);
    }
    s.trim()
        .trim_start_matches("crate::")
        .trim_start_matches("self::")
        .trim_start_matches("super::")
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != ':' && c != '.')
        .to_string()
}

fn clean_type_text(text: &str) -> String {
    text.trim()
        .trim_end_matches(',')
        .trim_end_matches(';')
        .trim()
        .to_string()
}

fn clean_ts_type_text(text: &str) -> String {
    clean_type_text(text)
        .trim_start_matches(':')
        .trim()
        .to_string()
}

fn split_type_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(normalize_type_name)
        .filter(|s| !s.is_empty())
        .collect()
}

fn leaf(name: &str) -> &str {
    name.rsplit("::")
        .next()
        .or_else(|| name.rsplit('.').next())
        .unwrap_or(name)
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}::{b}")
    }
}

fn node_text(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    node.utf8_text(bytes).ok().map(|s| s.to_string())
}

fn child_field_text(node: Node<'_>, field: &str, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| node_text(n, bytes))
}

fn module_for_file(file_path: &str) -> (String, String) {
    let path = Path::new(file_path);
    let crate_root = find_crate_root(path);
    let crate_name = crate_root
        .as_ref()
        .and_then(|root| read_package_name(&root.join("Cargo.toml")))
        .unwrap_or_else(|| "crate".to_string());
    let crate_ident = crate_name.replace('-', "_");
    let module = match &crate_root {
        Some(root) => {
            let src = root.join("src");
            match path.strip_prefix(&src).ok() {
                Some(rel) => {
                    let mut module = crate_ident.clone();
                    let comps: Vec<_> = rel.components().collect();
                    for (i, comp) in comps.iter().enumerate() {
                        let s = comp.as_os_str().to_string_lossy().to_string();
                        let is_last = i == comps.len() - 1;
                        if is_last {
                            let stem = s.trim_end_matches(".rs");
                            if !matches!(stem, "lib" | "mod" | "main") {
                                module = join(&module, stem);
                            }
                        } else {
                            module = join(&module, &s);
                        }
                    }
                    module
                }
                None => crate_ident.clone(),
            }
        }
        None => crate_ident.clone(),
    };
    (crate_ident, module)
}

fn find_crate_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("Cargo.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn read_package_name(cargo_toml: &Path) -> Option<String> {
    let text = fs::read_to_string(cargo_toml).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = t.strip_prefix("name") {
                let rest = rest.trim_start().strip_prefix('=')?.trim();
                let name = rest.trim_matches(|c| c == '"' || c == '\'').to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn ts_module_for_file(file_path: &str) -> (String, String) {
    let path = Path::new(file_path);
    let pkg_root = find_pkg_root(path);
    let pkg_ident = pkg_root
        .as_deref()
        .and_then(|r| read_package_json_name(&r.join("package.json")))
        .map(|n| sanitize_ident(n.rsplit('/').next().unwrap_or(&n)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pkg".to_string());
    let mut module = pkg_ident.clone();
    if let Some(root) = pkg_root.as_deref() {
        if let Ok(rel) = path.strip_prefix(root) {
            let comps: Vec<_> = rel.components().collect();
            for (i, comp) in comps.iter().enumerate() {
                let s = comp.as_os_str().to_string_lossy().to_string();
                let is_last = i == comps.len() - 1;
                if is_last {
                    let stem = trim_ts_ext(&s);
                    if stem != "index" {
                        module = join(&module, &sanitize_ident(&stem));
                    }
                } else if !(i == 0 && s == "src") {
                    module = join(&module, &sanitize_ident(&s));
                }
            }
        }
    }
    (pkg_ident, module)
}

fn find_pkg_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("package.json").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn read_package_json_name(pkg_json: &Path) -> Option<String> {
    let text = fs::read_to_string(pkg_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("name")?.as_str().map(|s| s.to_string())
}

fn sanitize_ident(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn trim_ts_ext(name: &str) -> String {
    for ext in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
        if let Some(stem) = name.strip_suffix(&format!(".{ext}")) {
            let stem = stem.strip_suffix(".d").unwrap_or(stem);
            return stem.to_string();
        }
    }
    name.to_string()
}
