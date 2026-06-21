use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde_json::json;
use sqlx::Row;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct ConfigExtractor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ConfigKind {
    Env,
    Secret,
    Flag,
}

#[derive(Debug, Clone)]
struct ConfigRead {
    kind: ConfigKind,
    key: String,
    confidence: f32,
    method: &'static str,
    line: i32,
    api: String,
}

#[derive(Debug, Clone)]
struct FunctionSpan {
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
                let consts = extract_string_consts(&source);
                let reads = extract_config_reads(&source, &consts);
                if reads.is_empty() {
                    continue;
                }
                let functions = load_file_functions(ctx, &path).await?;
                for read in reads {
                    if let Some(func) = enclosing_function(&functions, read.line) {
                        facts.add_read(func, read);
                    }
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

    fn add_read(&mut self, func: &FunctionSpan, read: ConfigRead) {
        let node_key = (read.kind, read.key.clone());
        self.nodes
            .entry(node_key)
            .and_modify(|c| *c = c.max(read.confidence))
            .or_insert(read.confidence);

        let dst_path = config_path(self.corpus, read.kind, &read.key);
        let edge_key = (
            func.path.clone(),
            dst_path.clone(),
            "reads_config".to_string(),
        );
        if !self.seen_edges.insert(edge_key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path: func.path.clone(),
            dst_path,
            edge_type: "reads_config".to_string(),
            confidence: read.confidence,
            provenance: "ast".to_string(),
            method: Some(read.method.to_string()),
            evidence: Some(json!({
                "line": read.line,
                "api": read.api,
                "kind": kind_node_type(read.kind),
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for ((kind, key), confidence) in &self.nodes {
            out.push(Fact::Node {
                path: config_path(self.corpus, *kind, key),
                title: key.clone(),
                node_type: kind_node_type(*kind).to_string(),
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

async fn load_file_functions(ctx: &ExtractCtx<'_>, path: &Path) -> Result<Vec<FunctionSpan>> {
    let path = path.to_string_lossy();
    let rows = sqlx::query(
        r#"WITH RECURSIVE descend(id) AS (
               SELECT e.dst_id
                 FROM brain_vault_nodes file_node
                 JOIN brain_vault_edges e ON e.src_id = file_node.id
                WHERE file_node.project = $1
                  AND file_node.path = $2
                  AND file_node.node_type = 'content:file'
                  AND e.edge_type = 'contains'
               UNION
               SELECT e.dst_id
                 FROM brain_vault_edges e
                 JOIN descend d ON e.src_id = d.id
                WHERE e.edge_type = 'contains'
           )
           SELECT fn_node.path, fn_node.start_line, fn_node.end_line
             FROM brain_vault_nodes fn_node
             JOIN descend d ON d.id = fn_node.id
            WHERE fn_node.node_type = 'code:function'
              AND fn_node.start_line IS NOT NULL
              AND fn_node.end_line IS NOT NULL
              AND COALESCE(fn_node.generation, 0) IN (0, $3)
            ORDER BY fn_node.start_line DESC"#,
    )
    .bind(ctx.corpus_slug)
    .bind(path.as_ref())
    .bind(ctx.generation)
    .fetch_all(ctx.pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| FunctionSpan {
            path: row.get("path"),
            start_line: row.get("start_line"),
            end_line: row.get("end_line"),
        })
        .collect())
}

fn enclosing_function(functions: &[FunctionSpan], line: i32) -> Option<&FunctionSpan> {
    functions
        .iter()
        .find(|f| f.start_line <= line && line <= f.end_line)
}

fn extract_config_reads(source: &str, consts: &HashMap<String, String>) -> Vec<ConfigRead> {
    let mut out = Vec::new();
    out.extend(extract_env_reads(source, consts));
    out.extend(extract_secret_reads(source, consts));
    out.extend(extract_flag_reads(source, consts));
    out
}

fn extract_env_reads(source: &str, consts: &HashMap<String, String>) -> Vec<ConfigRead> {
    let mut out = Vec::new();
    let env_re = Regex::new(r#"\b(?:std::)?env::var(?:_os)?\s*\("#).expect("valid env regex");
    for m in env_re.find_iter(source) {
        if let Some(args) = args_after_open_paren(source, m.end() - 1) {
            if let Some(arg) = split_top_level_args(args).first() {
                let (key, confidence, method) = resolve_key(arg, consts);
                out.push(ConfigRead {
                    kind: ConfigKind::Env,
                    key,
                    confidence,
                    method,
                    line: line_for_byte(source, m.start()),
                    api: source[m.start()..m.end()].trim_end_matches('(').to_string(),
                });
            }
        }
    }

    let option_re = Regex::new(r#"\boption_env!\s*\("#).expect("valid option_env regex");
    for m in option_re.find_iter(source) {
        if let Some(args) = args_after_open_paren(source, m.end() - 1) {
            if let Some(arg) = split_top_level_args(args).first() {
                let (key, confidence, method) = resolve_key(arg, consts);
                out.push(ConfigRead {
                    kind: ConfigKind::Env,
                    key,
                    confidence,
                    method,
                    line: line_for_byte(source, m.start()),
                    api: "option_env!".to_string(),
                });
            }
        }
    }
    out
}

fn extract_secret_reads(source: &str, consts: &HashMap<String, String>) -> Vec<ConfigRead> {
    let mut out = Vec::new();
    let call_re = Regex::new(
        r#"\b(?:(?:[A-Za-z_][A-Za-z0-9_]*::)*)?(fetch_secret|get_secret|pg_get_secret|pg_read_safety_gate|get_hf_token)\s*\("#,
    )
    .expect("valid secret regex");
    for caps in call_re.captures_iter(source) {
        let Some(call) = caps.get(0) else {
            continue;
        };
        let Some(func) = caps.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(args) = args_after_open_paren(source, call.end() - 1) else {
            continue;
        };
        let resolved = if func == "get_hf_token" {
            ("huggingface.token".to_string(), 0.9, "EXTRACTED")
        } else {
            let arg_idx = if matches!(func, "pg_get_secret" | "pg_read_safety_gate") {
                1
            } else {
                0
            };
            let args = split_top_level_args(args);
            let Some(arg) = args.get(arg_idx) else {
                continue;
            };
            resolve_key(arg, consts)
        };
        let kind = if func == "pg_read_safety_gate" || is_flag_key(&resolved.0) {
            ConfigKind::Flag
        } else {
            ConfigKind::Secret
        };
        out.push(ConfigRead {
            kind,
            key: resolved.0,
            confidence: resolved.1,
            method: resolved.2,
            line: line_for_byte(source, call.start()),
            api: func.to_string(),
        });
    }

    out.extend(extract_fleet_secret_sql_reads(source, consts));
    out
}

fn extract_flag_reads(source: &str, consts: &HashMap<String, String>) -> Vec<ConfigRead> {
    let mut out = Vec::new();
    let flag_re = Regex::new(r#"\b(?:(?:[A-Za-z_][A-Za-z0-9_]*::)*)?is_enabled\s*\("#)
        .expect("valid flag regex");
    for m in flag_re.find_iter(source) {
        if let Some(args) = args_after_open_paren(source, m.end() - 1) {
            if let Some(arg) = split_top_level_args(args).first() {
                let (key, confidence, method) = resolve_key(arg, consts);
                out.push(ConfigRead {
                    kind: ConfigKind::Flag,
                    key,
                    confidence,
                    method,
                    line: line_for_byte(source, m.start()),
                    api: "is_enabled".to_string(),
                });
            }
        }
    }
    out
}

fn extract_fleet_secret_sql_reads(
    source: &str,
    consts: &HashMap<String, String>,
) -> Vec<ConfigRead> {
    let mut out = Vec::new();
    let sql_lit_re = Regex::new(r##"(?s)sqlx::query(?:_as|_scalar)?\s*\(\s*(r#*".*?"#*|".*?")"##)
        .expect("valid sqlx regex");
    for caps in sql_lit_re.captures_iter(source) {
        let Some(call) = caps.get(0) else {
            continue;
        };
        let Some(sql_raw) = caps.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(sql) = unquote_rust_string(sql_raw) else {
            continue;
        };
        if !sql.contains("fleet_secrets") || !sql.to_ascii_lowercase().contains("select") {
            continue;
        }
        if let Some(key) = extract_literal_sql_secret_key(&sql) {
            out.push(ConfigRead {
                kind: ConfigKind::Secret,
                key,
                confidence: 0.9,
                method: "EXTRACTED",
                line: line_for_byte(source, call.start()),
                api: "sqlx::query fleet_secrets".to_string(),
            });
        } else if sql.contains("key = $1") {
            let after = &source[call.end()..source.len().min(call.end() + 500)];
            let key_arg = first_bind_arg(after);
            let (key, confidence, method) = key_arg
                .as_deref()
                .map(|arg| resolve_key(arg, consts))
                .unwrap_or_else(|| ("<dynamic:fleet_secrets.key>".to_string(), 0.5, "DYNAMIC"));
            out.push(ConfigRead {
                kind: if is_flag_key(&key) {
                    ConfigKind::Flag
                } else {
                    ConfigKind::Secret
                },
                key,
                confidence,
                method,
                line: line_for_byte(source, call.start()),
                api: "sqlx::query fleet_secrets".to_string(),
            });
        }
    }
    out
}

fn extract_string_consts(source: &str) -> HashMap<String, String> {
    let re = Regex::new(
        r##"(?m)\b(?:pub\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*(?:&str|&'static\s+str|String)\s*=\s*(r#*".*?"#*|".*?")\s*;"##,
    )
    .expect("valid const regex");
    re.captures_iter(source)
        .filter_map(|caps| {
            let name = caps.get(1)?.as_str().to_string();
            let value = unquote_rust_string(caps.get(2)?.as_str())?;
            Some((name, value))
        })
        .collect()
}

fn resolve_key(raw_arg: &str, consts: &HashMap<String, String>) -> (String, f32, &'static str) {
    let arg = raw_arg
        .trim()
        .trim_start_matches('&')
        .trim()
        .trim_end_matches(".as_str()")
        .trim();
    if let Some(s) = unquote_rust_string(arg) {
        return (s, 0.9, "EXTRACTED");
    }
    let ident = arg.rsplit("::").next().unwrap_or(arg);
    if let Some(value) = consts.get(ident) {
        return (value.clone(), 0.6, "EXTRACTED");
    }
    (arg.to_string(), 0.5, "DYNAMIC")
}

fn is_flag_key(key: &str) -> bool {
    key.ends_with("_mode")
        || key.ends_with("_enabled")
        || key == "leader_self_upgrade"
        || key == "auto_upgrade_enabled"
        || key.contains("feature")
}

fn first_bind_arg(source_after_call: &str) -> Option<String> {
    let bind_re = Regex::new(r#"\.bind\s*\("#).expect("valid bind regex");
    let m = bind_re.find(source_after_call)?;
    args_after_open_paren(source_after_call, m.end() - 1)
        .and_then(|args| split_top_level_args(args).first().cloned())
}

fn extract_literal_sql_secret_key(sql: &str) -> Option<String> {
    let re = Regex::new(r#"(?i)\bkey\s*=\s*'([^']+)'"#).expect("valid sql key regex");
    re.captures(sql)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

fn args_after_open_paren(source: &str, open_paren: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    if bytes.get(open_paren).copied()? != b'(' {
        return None;
    }
    let mut depth = 0usize;
    let mut in_str = false;
    let mut raw_hashes: Option<usize> = None;
    let mut escaped = false;
    let mut i = open_paren;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if raw_hashes.is_some() {
                if b == b'"' {
                    let hashes = raw_hashes.unwrap_or(0);
                    if source[i + 1..]
                        .as_bytes()
                        .iter()
                        .take(hashes)
                        .all(|c| *c == b'#')
                    {
                        in_str = false;
                        raw_hashes = None;
                        i += hashes;
                    }
                }
            } else if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'r' {
            let mut j = i + 1;
            while bytes.get(j).copied() == Some(b'#') {
                j += 1;
            }
            if bytes.get(j).copied() == Some(b'"') {
                in_str = true;
                raw_hashes = Some(j - i - 1);
                i = j + 1;
                continue;
            }
        }
        match b {
            b'"' => in_str = true,
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return source.get(open_paren + 1..i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in args.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(args[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

fn unquote_rust_string(raw: &str) -> Option<String> {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix('r') {
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        if rest.as_bytes().get(hashes).copied() == Some(b'"') {
            let body = rest.get(hashes + 1..rest.len().checked_sub(hashes + 1)?)?;
            return Some(body.to_string());
        }
    }
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        return Some(s[1..s.len() - 1].replace("\\\"", "\""));
    }
    None
}

fn line_for_byte(source: &str, byte: usize) -> i32 {
    source[..byte].bytes().filter(|b| *b == b'\n').count() as i32 + 1
}

fn config_path(corpus: &str, kind: ConfigKind, key: &str) -> String {
    let segment = match kind {
        ConfigKind::Env => "env",
        ConfigKind::Secret => "secret",
        ConfigKind::Flag => "flag",
    };
    format!("config://{corpus}/{segment}/{key}")
}

fn kind_node_type(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Env => "config:env",
        ConfigKind::Secret => "config:secret",
        ConfigKind::Flag => "config:flag",
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
