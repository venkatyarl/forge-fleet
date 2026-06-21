use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde_json::json;
use sqlx::Row;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct ConfigExtractor;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ConfigKind {
    Env,
    Secret,
    Flag,
}

#[derive(Debug, Clone)]
struct ConfigRead {
    kind: ConfigKind,
    key: String,
    function_path: String,
    confidence: f32,
    method: String,
    line: i32,
    api: String,
}

#[derive(Debug, Clone)]
struct SourceFunction {
    path: String,
    start_line: i32,
    end_line: i32,
}

#[async_trait::async_trait]
impl Extractor for ConfigExtractor {
    fn name(&self) -> &'static str {
        "config"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root, "rs")? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let file_path = path.to_string_lossy().to_string();
                let functions = load_file_functions(ctx, &file_path).await?;
                let consts = extract_string_consts(&source);
                for read in extract_config_reads(&source, &consts, &functions) {
                    facts.add_read(read);
                }
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    nodes: BTreeMap<(ConfigKind, String), f32>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_read(&mut self, read: ConfigRead) {
        let node_key = (read.kind.clone(), read.key.clone());
        self.nodes
            .entry(node_key)
            .and_modify(|confidence| *confidence = confidence.max(read.confidence))
            .or_insert(read.confidence);

        let dst_path = config_path(self.corpus, &read.kind, &read.key);
        let edge_key = (
            read.function_path.clone(),
            dst_path.clone(),
            "reads_config".to_string(),
        );
        if !self.seen_edges.insert(edge_key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path: read.function_path,
            dst_path,
            edge_type: "reads_config".to_string(),
            confidence: read.confidence,
            provenance: "ast".to_string(),
            method: Some(read.method),
            evidence: Some(json!({
                "line": read.line,
                "api": read.api,
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for ((kind, key), confidence) in &self.nodes {
            out.push(Fact::Node {
                path: config_path(self.corpus, kind, key),
                title: key.clone(),
                node_type: config_node_type(kind).to_string(),
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

async fn load_file_functions(ctx: &ExtractCtx<'_>, file_path: &str) -> Result<Vec<SourceFunction>> {
    let rows = sqlx::query(
        r#"WITH RECURSIVE down(id) AS (
               SELECT id
                 FROM brain_vault_nodes
                WHERE project = $1
                  AND path = $2
                  AND node_type = 'content:file'
                  AND valid_until IS NULL
               UNION
               SELECT e.dst_id
                 FROM brain_vault_edges e
                 JOIN down d ON d.id = e.src_id
                WHERE e.edge_type = 'contains'
           )
           SELECT n.path, n.start_line, n.end_line
             FROM down d
             JOIN brain_vault_nodes n ON n.id = d.id
            WHERE n.node_type = 'code:function'
              AND n.start_line IS NOT NULL
              AND n.end_line IS NOT NULL
              AND n.valid_until IS NULL"#,
    )
    .bind(ctx.corpus_slug)
    .bind(file_path)
    .fetch_all(ctx.pool)
    .await?;

    let mut functions = rows
        .into_iter()
        .map(|row| SourceFunction {
            path: row.get("path"),
            start_line: row.get("start_line"),
            end_line: row.get("end_line"),
        })
        .collect::<Vec<_>>();
    functions.sort_by_key(|f| (f.start_line, f.end_line - f.start_line));
    Ok(functions)
}

fn extract_config_reads(
    source: &str,
    consts: &HashMap<String, String>,
    functions: &[SourceFunction],
) -> Vec<ConfigRead> {
    let line_starts = line_start_offsets(source);
    let mut reads = Vec::new();
    let specs = call_specs();
    for spec in &specs {
        for mat in spec.regex.find_iter(source) {
            if looks_like_fn_definition(source, mat.start()) {
                continue;
            }
            let open = mat.end().saturating_sub(1);
            let Some(args) = extract_parenthesized(source, open) else {
                continue;
            };
            let args = split_args(args);
            let key_arg = if let Some(index) = spec.arg_index {
                args.get(index).copied()
            } else {
                Some("")
            };
            let Some(key_arg) = key_arg else {
                continue;
            };
            let (key, confidence, method) = match spec.fixed_key {
                Some(key) => (key.to_string(), 0.9, "EXTRACTED".to_string()),
                None => resolve_key(key_arg, consts),
            };
            let line = byte_to_line(&line_starts, mat.start());
            let Some(function_path) = enclosing_function(functions, line) else {
                continue;
            };
            reads.push(ConfigRead {
                kind: spec.kind.clone(),
                key,
                function_path,
                confidence,
                method,
                line,
                api: spec.api.to_string(),
            });
        }
    }

    reads.extend(extract_fleet_secret_sql_reads(
        source,
        &line_starts,
        functions,
    ));
    reads
}

fn looks_like_fn_definition(source: &str, at: usize) -> bool {
    let line_start = source[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    source[line_start..at]
        .split_whitespace()
        .last()
        .is_some_and(|token| token == "fn")
}

#[derive(Debug)]
struct CallSpec {
    regex: Regex,
    kind: ConfigKind,
    api: &'static str,
    arg_index: Option<usize>,
    fixed_key: Option<&'static str>,
}

fn call_specs() -> Vec<CallSpec> {
    vec![
        CallSpec {
            regex: Regex::new(r#"\b(?:std::env|env)::var(?:_os)?\s*\("#).expect("valid regex"),
            kind: ConfigKind::Env,
            api: "env::var",
            arg_index: Some(0),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\boption_env!\s*\("#).expect("valid regex"),
            kind: ConfigKind::Env,
            api: "option_env",
            arg_index: Some(0),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*fetch_secret\s*\("#)
                .expect("valid regex"),
            kind: ConfigKind::Secret,
            api: "fetch_secret",
            arg_index: Some(0),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*get_secret\s*\("#)
                .expect("valid regex"),
            kind: ConfigKind::Secret,
            api: "get_secret",
            arg_index: Some(0),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*pg_get_secret\s*\("#)
                .expect("valid regex"),
            kind: ConfigKind::Secret,
            api: "pg_get_secret",
            arg_index: Some(1),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*pg_read_safety_gate\s*\("#)
                .expect("valid regex"),
            kind: ConfigKind::Flag,
            api: "pg_read_safety_gate",
            arg_index: Some(1),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*is_enabled\s*\("#)
                .expect("valid regex"),
            kind: ConfigKind::Flag,
            api: "is_enabled",
            arg_index: Some(0),
            fixed_key: None,
        },
        CallSpec {
            regex: Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*get_hf_token\s*\("#)
                .expect("valid regex"),
            kind: ConfigKind::Secret,
            api: "get_hf_token",
            arg_index: None,
            fixed_key: Some("huggingface.token"),
        },
    ]
}

fn extract_fleet_secret_sql_reads(
    source: &str,
    line_starts: &[usize],
    functions: &[SourceFunction],
) -> Vec<ConfigRead> {
    let re = Regex::new(r#"(?s)fleet_secrets.{0,160}?\bkey\s*=\s*(?:'([^']+)'|"([^"]+)")"#)
        .expect("valid regex");
    let mut out = Vec::new();
    for caps in re.captures_iter(source) {
        let Some(mat) = caps.get(0) else {
            continue;
        };
        let Some(key) = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|m| m.as_str().to_string())
        else {
            continue;
        };
        let line = byte_to_line(line_starts, mat.start());
        let Some(function_path) = enclosing_function(functions, line) else {
            continue;
        };
        out.push(ConfigRead {
            kind: ConfigKind::Secret,
            key,
            function_path,
            confidence: 0.9,
            method: "EXTRACTED".to_string(),
            line,
            api: "fleet_secrets SQL".to_string(),
        });
    }
    out
}

fn resolve_key(arg: &str, consts: &HashMap<String, String>) -> (String, f32, String) {
    if let Some(lit) = rust_string_literal_value(arg.trim()) {
        return (lit, 0.9, "EXTRACTED".to_string());
    }

    let trimmed = arg.trim();
    if let Some(value) = consts.get(trimmed) {
        return (value.clone(), 0.9, "EXTRACTED".to_string());
    }
    if let Some(leaf) = trimmed.rsplit("::").next()
        && let Some(value) = consts.get(leaf)
    {
        return (value.clone(), 0.9, "EXTRACTED".to_string());
    }
    if looks_like_const_path(trimmed) {
        return (trimmed.to_string(), 0.6, "CONST_PATH".to_string());
    }
    (trimmed.to_string(), 0.5, "DYNAMIC".to_string())
}

fn looks_like_const_path(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | ':'))
        && s.chars().any(|c| c.is_ascii_uppercase())
}

fn enclosing_function(functions: &[SourceFunction], line: i32) -> Option<String> {
    functions
        .iter()
        .filter(|f| f.start_line <= line && line <= f.end_line)
        .min_by_key(|f| (f.end_line - f.start_line, f.start_line))
        .map(|f| f.path.clone())
}

fn extract_string_consts(source: &str) -> HashMap<String, String> {
    let re = Regex::new(
        r#"(?m)\b(?:pub\s+)?(?:const|static)\s+([A-Z][A-Z0-9_]*)\s*:\s*(?:&'static\s+str|&str|str)\s*=\s*([^;]+);"#,
    )
    .expect("valid regex");
    let mut out = HashMap::new();
    for caps in re.captures_iter(source) {
        let Some(name) = caps.get(1).map(|m| m.as_str().to_string()) else {
            continue;
        };
        let Some(value) = caps
            .get(2)
            .and_then(|m| rust_string_literal_value(m.as_str().trim()))
        else {
            continue;
        };
        out.insert(name, value);
    }
    out
}

fn rust_string_literal_value(s: &str) -> Option<String> {
    let s = s.trim();
    if s.starts_with("r#") || s.starts_with("r\"") {
        return raw_string_literal_value(s);
    }
    if !s.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = s[1..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => {
                if let Some(next) = chars.next() {
                    match next {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        other => out.push(other),
                    }
                }
            }
            other => out.push(other),
        }
    }
    None
}

fn raw_string_literal_value(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.first().copied() != Some(b'r') {
        return None;
    }
    let mut hashes = 0usize;
    while bytes.get(1 + hashes).copied() == Some(b'#') {
        hashes += 1;
    }
    if bytes.get(1 + hashes).copied() != Some(b'"') {
        return None;
    }
    let start = 2 + hashes;
    let end_marker = format!("\"{}", "#".repeat(hashes));
    s[start..]
        .find(&end_marker)
        .map(|end| s[start..start + end].to_string())
}

fn extract_parenthesized(source: &str, open: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    if bytes.get(open).copied() != Some(b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in source[open..].char_indices() {
        let idx = open + offset;
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
                    return Some(&source[open + 1..idx]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in args.char_indices() {
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
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(args[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn config_path(corpus: &str, kind: &ConfigKind, key: &str) -> String {
    format!("config://{corpus}/{}/{}", config_path_segment(kind), key)
}

fn config_node_type(kind: &ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Env => "config:env",
        ConfigKind::Secret => "config:secret",
        ConfigKind::Flag => "config:flag",
    }
}

fn config_path_segment(kind: &ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Env => "env",
        ConfigKind::Secret => "secret",
        ConfigKind::Flag => "flag",
    }
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
