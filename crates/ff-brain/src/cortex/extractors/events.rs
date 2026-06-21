use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde_json::json;
use sqlx::Row;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct EventsExtractor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Publishes,
    Subscribes,
}

#[derive(Debug, Clone)]
struct EventSite {
    kind: EventKind,
    subject: String,
    confidence: f32,
    method: &'static str,
    line: i32,
    call: String,
}

#[derive(Debug, Clone)]
struct FunctionSpan {
    path: String,
    start_line: i32,
    end_line: i32,
}

#[async_trait::async_trait]
impl Extractor for EventsExtractor {
    fn name(&self) -> &'static str {
        "events"
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
                if functions.is_empty() {
                    continue;
                }

                for site in find_event_sites(&source) {
                    let Some(function) = enclosing_function(&functions, site.line) else {
                        continue;
                    };
                    facts.add_event(function.path.clone(), site);
                }
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    topics: BTreeMap<String, f32>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            topics: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_event(&mut self, src_path: String, site: EventSite) {
        self.topics
            .entry(site.subject.clone())
            .and_modify(|confidence| *confidence = confidence.max(site.confidence))
            .or_insert(site.confidence);

        let dst_path = topic_path(self.corpus, &site.subject);
        let edge_type = match site.kind {
            EventKind::Publishes => "publishes",
            EventKind::Subscribes => "subscribes",
        };
        let key = (src_path.clone(), dst_path.clone(), edge_type.to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path,
            edge_type: edge_type.to_string(),
            confidence: site.confidence,
            provenance: "ast".to_string(),
            method: Some(site.method.to_string()),
            evidence: Some(json!({
                "line": site.line,
                "call": site.call,
                "subject": site.subject,
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for (subject, confidence) in &self.topics {
            out.push(Fact::Node {
                path: topic_path(self.corpus, subject),
                title: subject.clone(),
                node_type: "event:topic".to_string(),
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

fn find_event_sites(source: &str) -> Vec<EventSite> {
    let consts = extract_string_consts(source);
    let line_starts = line_start_offsets(source);
    let mut sites = Vec::new();

    for (name, kind) in event_call_names() {
        for at in find_name_occurrences(source, name) {
            if is_function_definition(source, at) {
                continue;
            }
            if name == "request" && !is_method_call(source, at) {
                continue;
            }
            let Some(args_start) = source[at + name.len()..]
                .char_indices()
                .find_map(|(i, ch)| (!ch.is_whitespace()).then_some((i, ch)))
            else {
                continue;
            };
            if args_start.1 != '(' {
                continue;
            }
            let open = at + name.len() + args_start.0;
            let Some(close) = matching_paren(source, open) else {
                continue;
            };
            let args = &source[open + 1..close];
            let Some(first_arg) = first_argument(args) else {
                continue;
            };
            let (subject, confidence, method) =
                resolve_subject(source, at, first_arg.trim(), &consts);
            if subject.is_empty() {
                continue;
            }
            sites.push(EventSite {
                kind,
                subject,
                confidence,
                method,
                line: byte_to_line(&line_starts, at),
                call: name.to_string(),
            });
        }
    }

    sites.sort_by_key(|s| (s.line, s.call.clone(), s.subject.clone()));
    sites
}

fn event_call_names() -> Vec<(&'static str, EventKind)> {
    vec![
        ("publish_with_reply", EventKind::Publishes),
        ("queue_subscribe", EventKind::Subscribes),
        ("publish_json", EventKind::Publishes),
        ("publish_raw", EventKind::Publishes),
        ("publish_js", EventKind::Publishes),
        ("publish", EventKind::Publishes),
        ("request", EventKind::Publishes),
        ("subscribe", EventKind::Subscribes),
        ("spawn_subscriber", EventKind::Subscribes),
    ]
}

fn find_name_occurrences(source: &str, name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(pos) = source[offset..].find(name) {
        let at = offset + pos;
        let before = source[..at].chars().next_back();
        let after = source[at + name.len()..].chars().next();
        let before_ok = before.is_none_or(|ch| !is_ident_char(ch));
        let after_ok = after.is_none_or(|ch| !is_ident_char(ch));
        if before_ok && after_ok {
            out.push(at);
        }
        offset = at + name.len();
    }
    out
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_method_call(source: &str, at: usize) -> bool {
    source[..at].chars().rev().find(|ch| !ch.is_whitespace()) == Some('.')
}

fn is_function_definition(source: &str, at: usize) -> bool {
    let prefix = &source[..at];
    let prefix = prefix.trim_end();
    prefix.ends_with("fn") || prefix.ends_with("async fn") || prefix.ends_with("pub fn")
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

fn first_argument(args: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in args.char_indices() {
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
            ',' if depth == 0 => return Some(&args[..i]),
            _ => {}
        }
    }
    let trimmed = args.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Strip trailing string conversions so `"x".to_string()` / `"x".to_owned()` /
/// `"x".into()` / `"x".as_str()` resolve to the literal `x` instead of being
/// treated as a dynamic expression (which produced duplicate, cruft-suffixed
/// topics like `"fleet.events.x".to_string()`).
fn strip_trailing_conversions(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        let trimmed = s
            .strip_suffix(".to_string()")
            .or_else(|| s.strip_suffix(".to_owned()"))
            .or_else(|| s.strip_suffix(".into()"))
            .or_else(|| s.strip_suffix(".as_str()"))
            .map(str::trim_end);
        match trimmed {
            Some(t) if t != s => s = t,
            _ => return s,
        }
    }
}

fn resolve_subject(
    source: &str,
    at: usize,
    arg: &str,
    consts: &HashMap<String, String>,
) -> (String, f32, &'static str) {
    let arg = strip_trailing_conversions(arg);
    if let Some(lit) = rust_string_literal_value(arg) {
        return (lit, 0.9, "EXTRACTED");
    }
    if let Some(value) = consts.get(arg) {
        return (value.clone(), 0.9, "EXTRACTED");
    }
    if is_const_like(arg) {
        return (arg.to_string(), 0.6, "DYNAMIC");
    }
    if is_ident(arg)
        && let Some(expr) = nearest_let_assignment(source, at, arg)
    {
        if let Some(lit) = rust_string_literal_value(&expr) {
            return (lit, 0.9, "EXTRACTED");
        }
        if let Some(value) = consts.get(expr.trim()) {
            return (value.clone(), 0.9, "EXTRACTED");
        }
        return (expr, 0.5, "DYNAMIC");
    }
    (arg.to_string(), 0.5, "DYNAMIC")
}

fn nearest_let_assignment(source: &str, at: usize, ident: &str) -> Option<String> {
    let before = &source[..at];
    let re = Regex::new(&format!(
        r#"(?m)\blet\s+{}\s*=\s*(?P<expr>[^;\n]+);"#,
        regex::escape(ident)
    ))
    .ok()?;
    re.captures_iter(before)
        .filter_map(|cap| cap.name("expr").map(|m| m.as_str().trim().to_string()))
        .last()
}

fn extract_string_consts(source: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let re = Regex::new(
        r#"(?m)\b(?:pub\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*(?:&'static\s+str|&str)\s*=\s*("([^"\\]|\\.)*")"#,
    )
    .expect("valid const regex");
    for cap in re.captures_iter(source) {
        if let (Some(name), Some(value)) = (cap.get(1), cap.get(2))
            && let Some(lit) = rust_string_literal_value(value.as_str())
        {
            out.insert(name.as_str().to_string(), lit);
        }
    }
    out
}

fn rust_string_literal_value(expr: &str) -> Option<String> {
    let s = expr.trim();
    if !(s.starts_with('"') && s.ends_with('"') && s.len() >= 2) {
        return None;
    }
    serde_json::from_str::<String>(s).ok()
}

fn is_const_like(arg: &str) -> bool {
    arg.chars()
        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_ident(arg: &str) -> bool {
    let mut chars = arg.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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

fn topic_path(corpus: &str, subject: &str) -> String {
    format!("event://{corpus}/{subject}")
}
