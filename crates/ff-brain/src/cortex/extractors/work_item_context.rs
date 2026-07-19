//! Precompute context for `work_items`.
//!
//! Combines explicit signals from a work item (`predicted_paths`,
//! `brain_node_ids`) with Cortex graph search over its title and description to
//! surface:
//!
//! * relevant files (`content:file` nodes),
//! * relevant code snippets (`code:*` symbol spans),
//! * related work items (same project, shared labels, shared brain nodes).
//!
//! The [`WorkItemContextExtractor`] plugs into the standard extractor SPI and
//! emits `relevant_to` / `related_to` edges between `pm:work_item` nodes and the
//! code/content graph during a corpus reindex. The standalone functions can also
//! be called directly (e.g. by dispatch) to build a prompt context bundle.

use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use uuid::Uuid;

pub struct WorkItemContextExtractor;

/// A file that is relevant to a work item.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelevantFile {
    pub path: String,
    pub title: String,
    pub score: f32,
}

/// A code snippet that is relevant to a work item.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodeSnippet {
    pub symbol_path: String,
    pub title: String,
    pub file_path: String,
    pub start_line: i32,
    pub end_line: i32,
    pub snippet: String,
    pub truncated: bool,
}

/// Another work item that is related to the target work item.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelatedWorkItem {
    pub id: Uuid,
    pub title: String,
    pub reason: String,
    pub score: f32,
}

/// Precomputed context bundle for a single work item.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkItemContext {
    pub work_item_id: Uuid,
    pub files: Vec<RelevantFile>,
    pub snippets: Vec<CodeSnippet>,
    pub related: Vec<RelatedWorkItem>,
}

impl WorkItemContext {
    /// All brain node paths referenced by this context (files, symbols, related
    /// work items). Useful for populating `work_items.brain_node_ids`.
    pub fn brain_node_paths(&self) -> Vec<String> {
        let mut out =
            Vec::with_capacity(self.files.len() + self.snippets.len() + self.related.len());
        for file in &self.files {
            out.push(file.path.clone());
        }
        for snippet in &self.snippets {
            out.push(snippet.symbol_path.clone());
        }
        for related in &self.related {
            out.push(pm_work_item_path(related.id));
        }
        out.sort();
        out.dedup();
        out
    }
}

#[async_trait::async_trait]
impl Extractor for WorkItemContextExtractor {
    fn name(&self) -> &'static str {
        "work_item_context"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let items = load_work_items_for_project(ctx.pool, ctx.corpus_slug).await?;
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let mut facts = Vec::new();
        for item in &items {
            let computed = compute_context(ctx.pool, item, ctx.corpus_slug).await?;
            for file in &computed.files {
                facts.push(relevant_to_fact(
                    &pm_work_item_path(item.id),
                    &file.path,
                    "file",
                    file.score,
                ));
            }
            for snippet in &computed.snippets {
                facts.push(relevant_to_fact(
                    &pm_work_item_path(item.id),
                    &snippet.symbol_path,
                    "symbol",
                    0.85,
                ));
            }
            for related in &computed.related {
                facts.push(related_to_fact(
                    &pm_work_item_path(item.id),
                    &pm_work_item_path(related.id),
                    related.score,
                ));
            }
        }

        Ok(facts)
    }
}

/// Read a work item from Postgres and compute its full context bundle.
pub async fn precompute_context(pool: &PgPool, work_item_id: Uuid) -> Result<WorkItemContext> {
    let item = load_work_item(pool, work_item_id)
        .await?
        .with_context(|| format!("work item {work_item_id} not found"))?;
    compute_context(pool, &item, &item.project_id).await
}

/// Find files relevant to a work item.
pub async fn relevant_files(
    pool: &PgPool,
    work_item_id: Uuid,
    limit: usize,
) -> Result<Vec<RelevantFile>> {
    let item = load_work_item(pool, work_item_id)
        .await?
        .with_context(|| format!("work item {work_item_id} not found"))?;
    rank_files(pool, &item, limit).await
}

/// Find code snippets relevant to a work item.
pub async fn relevant_code_snippets(
    pool: &PgPool,
    work_item_id: Uuid,
    limit: usize,
) -> Result<Vec<CodeSnippet>> {
    let item = load_work_item(pool, work_item_id)
        .await?
        .with_context(|| format!("work item {work_item_id} not found"))?;
    rank_code_snippets(pool, &item, &item.project_id, limit).await
}

/// Find work items related to a work item.
pub async fn related_work_items(
    pool: &PgPool,
    work_item_id: Uuid,
    limit: usize,
) -> Result<Vec<RelatedWorkItem>> {
    let item = load_work_item(pool, work_item_id)
        .await?
        .with_context(|| format!("work item {work_item_id} not found"))?;
    rank_related_work_items(pool, &item, limit).await
}

/// Persist a precomputed context back to the `work_items` row.
///
/// Updates `brain_node_ids` with the union of node paths from the context and
/// `touched_paths` with the file paths. Does not overwrite `predicted_paths`
/// (that is owned by the decomposer).
pub async fn write_context(
    pool: &PgPool,
    work_item_id: Uuid,
    context: &WorkItemContext,
) -> Result<()> {
    let node_paths: Vec<String> = context.brain_node_paths();
    let touched_paths: Vec<String> = context
        .files
        .iter()
        .map(|f| f.path.clone())
        .chain(context.snippets.iter().map(|s| s.file_path.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    sqlx::query(
        r#"
        UPDATE work_items
           SET brain_node_ids = COALESCE(brain_node_ids, '[]'::jsonb) || $2::jsonb,
               touched_paths = COALESCE(touched_paths, '[]'::jsonb) || $3::jsonb,
               updated_at = NOW()
         WHERE id = $1
        "#,
    )
    .bind(work_item_id)
    .bind(json!(node_paths))
    .bind(json!(touched_paths))
    .execute(pool)
    .await
    .with_context(|| format!("failed writing context for work item {work_item_id}"))?;

    Ok(())
}

#[derive(Debug, Clone)]
struct WorkItemRow {
    id: Uuid,
    project_id: String,
    title: String,
    description: Option<String>,
    labels: Vec<String>,
    predicted_paths: Vec<String>,
    brain_node_ids: Vec<String>,
    parent_id: Option<Uuid>,
}

impl WorkItemRow {
    fn pm_path(&self) -> String {
        pm_work_item_path(self.id)
    }

    /// Text used for keyword search against the graph.
    fn search_text(&self) -> String {
        let mut text = self.title.clone();
        if let Some(desc) = &self.description {
            text.push('\n');
            text.push_str(desc);
        }
        text
    }

    /// Distinct lower-case keywords extracted from the title and description.
    /// Filters out very short tokens to reduce noise.
    fn keywords(&self) -> Vec<String> {
        extract_keywords(&self.search_text())
    }
}

async fn load_work_item(pool: &PgPool, work_item_id: Uuid) -> Result<Option<WorkItemRow>> {
    let row = sqlx::query(
        r#"
        SELECT id,
               project_id,
               title,
               description,
               labels,
               COALESCE(predicted_paths, '[]'::jsonb) AS predicted_paths,
               COALESCE(brain_node_ids, '[]'::jsonb) AS brain_node_ids,
               parent_id
          FROM work_items
         WHERE id = $1
        "#,
    )
    .bind(work_item_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(parse_work_item_row).transpose()?)
}

async fn load_work_items_for_project(pool: &PgPool, project_id: &str) -> Result<Vec<WorkItemRow>> {
    let rows = sqlx::query(
        r#"
        SELECT id,
               project_id,
               title,
               description,
               labels,
               COALESCE(predicted_paths, '[]'::jsonb) AS predicted_paths,
               COALESCE(brain_node_ids, '[]'::jsonb) AS brain_node_ids,
               parent_id
          FROM work_items
         WHERE project_id = $1
         ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(parse_work_item_row).collect()
}

fn parse_work_item_row(row: sqlx::postgres::PgRow) -> Result<WorkItemRow> {
    Ok(WorkItemRow {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        title: row.try_get("title")?,
        description: row.try_get("description").ok(),
        labels: json_to_string_array(row.try_get("labels").unwrap_or_else(|_| json!([]))),
        predicted_paths: json_to_string_array(
            row.try_get("predicted_paths").unwrap_or_else(|_| json!([])),
        ),
        brain_node_ids: json_to_string_array(
            row.try_get("brain_node_ids").unwrap_or_else(|_| json!([])),
        ),
        parent_id: row.try_get("parent_id").ok(),
    })
}

fn json_to_string_array(value: Value) -> Vec<String> {
    match value {
        Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) if !s.trim().is_empty() => Some(s),
                _ => None,
            })
            .collect(),
        Value::String(s) if !s.trim().is_empty() => vec![s],
        _ => Vec::new(),
    }
}

async fn compute_context(
    pool: &PgPool,
    item: &WorkItemRow,
    corpus_slug: &str,
) -> Result<WorkItemContext> {
    let files = rank_files(pool, item, 16).await?;
    let snippets = rank_code_snippets(pool, item, corpus_slug, 16).await?;
    let related = rank_related_work_items(pool, item, 8).await?;

    Ok(WorkItemContext {
        work_item_id: item.id,
        files,
        snippets,
        related,
    })
}

async fn rank_files(pool: &PgPool, item: &WorkItemRow, limit: usize) -> Result<Vec<RelevantFile>> {
    let mut scores: HashMap<String, f32> = HashMap::new();

    // Signal 1: predicted_paths are the strongest explicit signal.
    for path in &item.predicted_paths {
        if let Some(file_path) = find_file_node_by_path(pool, &item.project_id, path).await? {
            *scores.entry(file_path).or_insert(0.0) += 1.0;
        }
    }

    // Signal 2: already-linked brain node IDs that are content:file paths.
    for node_path in &item.brain_node_ids {
        if is_content_file_path(pool, &item.project_id, node_path).await? {
            *scores.entry(node_path.clone()).or_insert(0.0) += 0.9;
        }
    }

    // Signal 3: keyword search against file titles/paths in the corpus.
    let keywords = item.keywords();
    if !keywords.is_empty() {
        let pattern = format!("%{}%", keywords.join("%"));
        let rows = sqlx::query(
            r#"
            SELECT path, title
              FROM brain_vault_nodes
             WHERE project = $1
               AND node_type = 'content:file'
               AND valid_until IS NULL
               AND (title ILIKE $2 OR path ILIKE $2)
             LIMIT 100
            "#,
        )
        .bind(&item.project_id)
        .bind(&pattern)
        .fetch_all(pool)
        .await?;

        for row in rows {
            let path: String = row.try_get("path")?;
            let title: String = row.try_get("title")?;
            let boost = keyword_match_boost(&title, &keywords) * 0.4
                + keyword_match_boost(&path, &keywords) * 0.2;
            if boost > 0.0 {
                *scores.entry(path).or_insert(0.0) += boost;
            }
        }
    }

    // Sort by score, then path for stability, and return the top N.
    let mut ranked: Vec<RelevantFile> = scores
        .into_iter()
        .map(|(path, score)| {
            let title = Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            RelevantFile { path, title, score }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    ranked.truncate(limit.max(1));
    Ok(ranked)
}

async fn rank_code_snippets(
    pool: &PgPool,
    item: &WorkItemRow,
    corpus_slug: &str,
    limit: usize,
) -> Result<Vec<CodeSnippet>> {
    let mut candidates: HashMap<String, (String, f32)> = HashMap::new();

    // Signal 1: brain_node_ids that point at code nodes.
    for node_path in &item.brain_node_ids {
        if node_path.starts_with("code://") {
            candidates
                .entry(node_path.clone())
                .or_insert_with(|| (String::new(), 0.0))
                .1 += 1.0;
        }
    }

    // Signal 2: keyword search over symbol titles in the corpus.
    let keywords = item.keywords();
    if !keywords.is_empty() {
        let pattern = format!("%{}%", keywords.join("%"));
        let rows = sqlx::query(
            r#"
            SELECT path, title
              FROM brain_vault_nodes
             WHERE project = $1
               AND node_type LIKE 'code:%'
               AND valid_until IS NULL
               AND title ILIKE $2
             LIMIT 100
            "#,
        )
        .bind(corpus_slug)
        .bind(&pattern)
        .fetch_all(pool)
        .await?;

        for row in rows {
            let path: String = row.try_get("path")?;
            let title: String = row.try_get("title")?;
            let boost = keyword_match_boost(&title, &keywords) * 0.5;
            if boost > 0.0 {
                candidates.entry(path).or_insert_with(|| (title, 0.0)).1 += boost;
            }
        }
    }

    // Resolve each candidate to a file span and read the snippet.
    let mut scored: Vec<(f32, CodeSnippet)> = Vec::new();
    for (symbol_path, (title, score)) in candidates {
        if let Some(span) = resolve_symbol_span(pool, corpus_slug, &symbol_path).await? {
            let snippet_text = match std::fs::read_to_string(&span.file_path) {
                Ok(source) => slice_source_lines(
                    &source,
                    span.start_line,
                    span.end_line,
                    120, // max_lines
                ),
                Err(_) => (String::new(), false),
            };
            scored.push((
                score,
                CodeSnippet {
                    symbol_path,
                    title: if title.is_empty() { span.title } else { title },
                    file_path: span.file_path,
                    start_line: span.start_line,
                    end_line: span.end_line,
                    snippet: snippet_text.0,
                    truncated: snippet_text.1,
                },
            ));
        }
    }

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.symbol_path.cmp(&b.1.symbol_path))
    });
    let mut snippets: Vec<CodeSnippet> = scored.into_iter().map(|(_, s)| s).collect();
    snippets.truncate(limit.max(1));
    Ok(snippets)
}

async fn rank_related_work_items(
    pool: &PgPool,
    item: &WorkItemRow,
    limit: usize,
) -> Result<Vec<RelatedWorkItem>> {
    let rows = sqlx::query(
        r#"
        SELECT id,
               title,
               COALESCE(labels, '[]'::jsonb) AS labels,
               COALESCE(brain_node_ids, '[]'::jsonb) AS brain_node_ids,
               parent_id
          FROM work_items
         WHERE id != $1
           AND project_id = $2
         ORDER BY created_at DESC
         LIMIT 200
        "#,
    )
    .bind(item.id)
    .bind(&item.project_id)
    .fetch_all(pool)
    .await?;

    let item_labels: HashSet<String> = item.labels.iter().cloned().collect();
    let item_brain: HashSet<String> = item.brain_node_ids.iter().cloned().collect();

    let mut scored = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let title: String = row.try_get("title")?;
        let labels: Vec<String> =
            json_to_string_array(row.try_get("labels").unwrap_or_else(|_| json!([])));
        let brain: Vec<String> =
            json_to_string_array(row.try_get("brain_node_ids").unwrap_or_else(|_| json!([])));
        let parent_id: Option<Uuid> = row.try_get("parent_id").ok();

        let mut score = 0.0f32;
        let mut reasons = Vec::new();

        // Same parent.
        if parent_id.is_some() && parent_id == item.parent_id {
            score += 0.5;
            reasons.push("shared parent");
        }

        // Shared labels.
        let shared_labels: HashSet<String> = labels.iter().cloned().collect();
        let label_overlap: Vec<_> = item_labels.intersection(&shared_labels).collect();
        if !label_overlap.is_empty() {
            score += 0.3 * label_overlap.len().min(3) as f32;
            reasons.push("shared labels");
        }

        // Shared brain nodes.
        let brain_set: HashSet<String> = brain.into_iter().collect();
        let node_overlap: Vec<_> = item_brain.intersection(&brain_set).collect();
        if !node_overlap.is_empty() {
            score += 0.4 * node_overlap.len().min(5) as f32;
            reasons.push("shared brain nodes");
        }

        // Title keyword overlap.
        let title_keywords: HashSet<String> = extract_keywords(&title).into_iter().collect();
        let own_keywords: HashSet<String> = item.keywords().into_iter().collect();
        let kw_overlap: Vec<_> = own_keywords.intersection(&title_keywords).collect();
        if !kw_overlap.is_empty() {
            score += 0.1 * kw_overlap.len().min(5) as f32;
            reasons.push("title keywords");
        }

        if score > 0.0 {
            reasons.sort();
            reasons.dedup();
            scored.push(RelatedWorkItem {
                id,
                title,
                reason: reasons.join(", "),
                score,
            });
        }
    }

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.title.cmp(&b.title))
    });
    scored.truncate(limit.max(1));
    Ok(scored)
}

async fn find_file_node_by_path(
    pool: &PgPool,
    project_id: &str,
    path: &str,
) -> Result<Option<String>> {
    // Match either the exact absolute path or a path ending with the relative
    // predicted path (the decomposer often emits repo-relative paths).
    let pattern = format!("%/{}", path.trim_start_matches('/'));
    let exact = path.trim().to_string();

    let row = sqlx::query(
        r#"
        SELECT path
          FROM brain_vault_nodes
         WHERE project = $1
           AND node_type = 'content:file'
           AND valid_until IS NULL
           AND (path = $2 OR path LIKE $3)
         ORDER BY CASE WHEN path = $2 THEN 0 ELSE 1 END
         LIMIT 1
        "#,
    )
    .bind(project_id)
    .bind(&exact)
    .bind(&pattern)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.try_get("path")).transpose()?)
}

async fn is_content_file_path(pool: &PgPool, project_id: &str, node_path: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
          FROM brain_vault_nodes
         WHERE project = $1
           AND path = $2
           AND node_type = 'content:file'
           AND valid_until IS NULL
        "#,
    )
    .bind(project_id)
    .bind(node_path)
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

#[derive(Debug, Clone)]
struct SymbolSpan {
    #[allow(dead_code)]
    title: String,
    file_path: String,
    start_line: i32,
    end_line: i32,
}

async fn resolve_symbol_span(
    pool: &PgPool,
    corpus_slug: &str,
    symbol_path: &str,
) -> Result<Option<SymbolSpan>> {
    let row = sqlx::query(
        r#"
        SELECT n.title,
               f.path AS file_path,
               n.start_line,
               n.end_line
          FROM brain_vault_nodes n
          JOIN brain_vault_edges e
            ON e.dst_id = n.id
           AND e.edge_type = 'contains'
          JOIN brain_vault_nodes f
            ON f.id = e.src_id
           AND f.node_type = 'content:file'
         WHERE n.path = $1
           AND n.project = $2
           AND n.valid_until IS NULL
           AND f.valid_until IS NULL
         LIMIT 1
        "#,
    )
    .bind(symbol_path)
    .bind(corpus_slug)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => Ok(Some(SymbolSpan {
            title: r.try_get("title")?,
            file_path: r.try_get("file_path")?,
            start_line: r.try_get::<Option<i32>, _>("start_line")?.unwrap_or(1),
            end_line: r.try_get::<Option<i32>, _>("end_line")?.unwrap_or(1),
        })),
        None => Ok(None),
    }
}

fn relevant_to_fact(src_path: &str, dst_path: &str, kind: &str, confidence: f32) -> Fact {
    Fact::Edge {
        src_path: src_path.to_string(),
        dst_path: dst_path.to_string(),
        edge_type: "relevant_to".to_string(),
        confidence,
        provenance: "work-item-context".to_string(),
        method: Some(kind.to_string()),
        evidence: Some(json!({ "kind": kind })),
    }
}

fn related_to_fact(src_path: &str, dst_path: &str, confidence: f32) -> Fact {
    Fact::Edge {
        src_path: src_path.to_string(),
        dst_path: dst_path.to_string(),
        edge_type: "related_to".to_string(),
        confidence,
        provenance: "work-item-context".to_string(),
        method: Some("work_item".to_string()),
        evidence: None,
    }
}

fn pm_work_item_path(id: Uuid) -> String {
    format!("pm://work_item/{id}")
}

fn extract_keywords(text: &str) -> Vec<String> {
    let mut out = HashSet::new();
    for token in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let lower = token.to_ascii_lowercase();
        if lower.len() >= 3 && !is_stop_word(&lower) {
            out.insert(lower);
        }
    }
    out.into_iter().collect()
}

fn is_stop_word(word: &str) -> bool {
    const STOP: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her", "was", "one",
        "our", "out", "day", "get", "has", "him", "his", "how", "its", "may", "new", "now", "old",
        "see", "two", "who", "boy", "did", "she", "use", "her", "way", "many", "oil", "sit", "set",
        "run", "eat", "far", "sea", "eye", "ask", "own", "say", "too", "any", "try", "let", "put",
        "end", "why", "turn", "here", "show", "every", "good", "me", "give", "most", "very",
        "when", "much", "would", "there", "their", "said", "each", "which", "will", "about",
        "could", "other", "after", "first", "never", "these", "think", "where", "being", "every",
        "great", "might", "shall", "still", "those", "while", "this", "that", "with", "have",
        "from", "they", "been", "were", "said", "time", "than", "them", "into", "just", "like",
        "over", "also", "back", "only", "know", "take", "year", "good", "some", "come", "make",
        "well", "work", "life", "even", "more", "want", "here", "look", "down", "most", "long",
        "last", "find", "give", "does", "made", "part", "such", "keep", "call", "came", "need",
        "feel", "seem", "turn", "hand", "high", "sure", "upon", "head", "help", "home", "side",
        "move", "both", "five", "once", "same", "must", "name", "left", "each", "done", "open",
        "case", "show", "live", "play", "went", "told", "seen", "hear", "talk", "soon", "read",
        "stop", "face", "fact", "land", "line", "kind", "next", "word", "came", "went", "told",
        "seen", "hear", "talk", "soon", "read", "stop", "face", "fact", "land", "line", "kind",
        "next", "word",
    ];
    STOP.contains(&word)
}

fn keyword_match_boost(text: &str, keywords: &[String]) -> f32 {
    let lower = text.to_ascii_lowercase();
    let hits = keywords.iter().filter(|k| lower.contains(*k)).count();
    if hits == 0 {
        0.0
    } else {
        // Diminishing returns so a single strong keyword doesn't swamp others.
        1.0 - 0.7f32.powi(hits as i32)
    }
}

/// Extract the 1-based inclusive line range `[start, end]` from `source`,
/// capped at `max_lines` lines. Returns `(slice, truncated)`. Pure so it is
/// unit-testable without touching the filesystem.
fn slice_source_lines(source: &str, start: i32, end: i32, max_lines: usize) -> (String, bool) {
    let start = start.max(1) as usize;
    let end = end.max(start as i32) as usize;
    let want = end.saturating_sub(start).saturating_add(1);
    let take = want.min(max_lines.max(1));
    let truncated = want > take;
    let slice: String = source
        .lines()
        .skip(start.saturating_sub(1))
        .take(take)
        .collect::<Vec<_>>()
        .join("\n");
    (slice, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_keywords_skips_short_and_stop_words() {
        let text = "Implement the user authentication service for login and logout";
        let mut kw = extract_keywords(text);
        kw.sort();
        assert!(kw.contains(&"authentication".to_string()));
        assert!(kw.contains(&"service".to_string()));
        assert!(kw.contains(&"login".to_string()));
        assert!(kw.contains(&"logout".to_string()));
        assert!(kw.contains(&"implement".to_string()));
        assert!(!kw.contains(&"the".to_string()));
        assert!(!kw.contains(&"and".to_string()));
        assert!(!kw.contains(&"for".to_string()));
    }

    #[test]
    fn keyword_match_boost_increases_with_hits() {
        let keywords = vec!["user".to_string(), "auth".to_string()];
        assert_eq!(keyword_match_boost("nothing here", &keywords), 0.0);
        let one = keyword_match_boost("user profile", &keywords);
        let two = keyword_match_boost("user auth flow", &keywords);
        assert!(two > one);
        assert!(two < 1.0);
    }

    #[test]
    fn slice_source_lines_extracts_range_and_caps() {
        let src = "l1\nl2\nl3\nl4\nl5";
        let (s, trunc) = slice_source_lines(src, 2, 4, 100);
        assert_eq!(s, "l2\nl3\nl4");
        assert!(!trunc);

        let (s, trunc) = slice_source_lines(src, 1, 5, 2);
        assert_eq!(s, "l1\nl2");
        assert!(trunc);

        let (s, _) = slice_source_lines(src, 0, -5, 0);
        assert_eq!(s, "l1");
    }

    #[test]
    fn brain_node_paths_deduplicates() {
        let ctx = WorkItemContext {
            work_item_id: Uuid::nil(),
            files: vec![RelevantFile {
                path: "file:///a.rs".to_string(),
                title: "a.rs".to_string(),
                score: 1.0,
            }],
            snippets: vec![CodeSnippet {
                symbol_path: "code://c/a".to_string(),
                title: "a".to_string(),
                file_path: "file:///a.rs".to_string(),
                start_line: 1,
                end_line: 2,
                snippet: String::new(),
                truncated: false,
            }],
            related: vec![RelatedWorkItem {
                id: Uuid::nil(),
                title: "other".to_string(),
                reason: "shared labels".to_string(),
                score: 0.5,
            }],
        };
        let paths = ctx.brain_node_paths();
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&"file:///a.rs".to_string()));
        assert!(paths.contains(&"code://c/a".to_string()));
        assert!(paths.contains(&pm_work_item_path(Uuid::nil())));
    }

    #[test]
    fn json_to_string_array_handles_mixed_input() {
        assert_eq!(
            json_to_string_array(json!(["a", "", "b", 1, null])),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            json_to_string_array(json!("single")),
            vec!["single".to_string()]
        );
        assert!(json_to_string_array(json!(123)).is_empty());
    }
}
