//! Obsidian vault parser + indexer.
//!
//! Parses markdown files with YAML frontmatter, extracts wikilinks,
//! performs hierarchical chunking, and upserts into Postgres.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Configuration for an Obsidian vault to index.
pub struct VaultConfig {
    /// Root path of the vault on disk, e.g. ~/projects/Yarli_KnowledgeBase
    pub vault_path: PathBuf,
    /// Subfolder within the vault to scope indexing, e.g. "Virtual Brain"
    pub brain_subfolder: String,
}

impl VaultConfig {
    /// Returns the full path to the brain subfolder.
    pub fn brain_root(&self) -> PathBuf {
        self.vault_path.join(&self.brain_subfolder)
    }
}

/// A parsed Obsidian markdown node (one .md file).
pub struct ParsedNode {
    pub path: String,
    pub title: String,
    pub node_type: Option<String>,
    pub tags: Vec<String>,
    pub extends_path: Option<String>,
    pub applies_to: Vec<String>,
    pub from_thread: Option<String>,
    pub confidence: Option<f32>,
    pub body: String,
    pub wikilinks: Vec<String>,
    pub content_hash: String,
}

/// A chunk of markdown text with its heading breadcrumb.
pub struct VaultChunk {
    /// e.g. "Projects/ForgeFleet/UI Design.md > Overrides > Color palette"
    pub breadcrumb: String,
    pub text: String,
    pub char_offset: usize,
    pub token_estimate: usize,
}

/// Summary of an indexing run.
pub struct IndexReport {
    pub files_scanned: usize,
    pub nodes_upserted: usize,
    pub edges_created: usize,
    pub chunks_written: usize,
    pub unchanged_skipped: usize,
}

/// Parse YAML frontmatter from a markdown file.
/// Returns (frontmatter as JSON Value, body without frontmatter).
pub fn parse_frontmatter(content: &str) -> (serde_json::Value, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (serde_json::Value::Null, content.to_string());
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let close_pos = after_first.find("\n---");
    match close_pos {
        Some(pos) => {
            let yaml_str = &after_first[..pos];
            let body_start = 3 + pos + 4; // skip "---" + "\n---"
            let body = trimmed[body_start..].trim_start_matches('\n').to_string();

            let fm: serde_json::Value = match serde_yaml::from_str(yaml_str) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse YAML frontmatter: {e}");
                    serde_json::Value::Null
                }
            };
            (fm, body)
        }
        None => (serde_json::Value::Null, content.to_string()),
    }
}

/// Extract `[[wikilinks]]` from markdown body. Returns list of target page names.
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    let re = regex::Regex::new(r"\[\[([^\]]+)\]\]").expect("valid regex");
    re.captures_iter(body)
        .map(|cap| {
            let target = cap[1].to_string();
            // Handle [[target|alias]] — return only the target part
            if let Some(pipe_pos) = target.find('|') {
                target[..pipe_pos].to_string()
            } else {
                target
            }
        })
        .collect()
}

/// Parse a single .md file into a ParsedNode.
pub fn parse_vault_file(path: &Path, vault_root: &Path) -> Result<ParsedNode, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    let relative = path
        .strip_prefix(vault_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let (fm, body) = parse_frontmatter(&content);
    let wikilinks = extract_wikilinks(&body);

    // Hash the full file content for change detection
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize());

    // Extract fields from frontmatter
    let node_type = fm.get("type").and_then(|v| v.as_str()).map(String::from);
    let tags: Vec<String> = match fm.get("tags") {
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        }
        Some(serde_json::Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).collect(),
        _ => Vec::new(),
    };
    let extends_path = fm.get("extends").and_then(|v| v.as_str()).map(String::from);
    let applies_to: Vec<String> = match fm.get("applies_to") {
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        }
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    let from_thread = fm.get("from_thread").and_then(|v| v.as_str()).map(String::from);
    let confidence = fm
        .get("confidence")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);

    Ok(ParsedNode {
        path: relative,
        title,
        node_type,
        tags,
        extends_path,
        applies_to,
        from_thread,
        confidence,
        body,
        wikilinks,
        content_hash,
    })
}

/// Hierarchical chunking: split by headings first, then recursive 512-token
/// with ~20% overlap. Each chunk carries its heading breadcrumb.
pub fn chunk_markdown(body: &str, file_path: &str) -> Vec<VaultChunk> {
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new(); // (level, text)
    let mut current_text = String::new();
    let mut section_start: usize = 0;

    let lines: Vec<&str> = body.lines().collect();
    let max_chunk_chars = 512 * 4; // ~512 tokens at 4 chars/token
    let overlap_chars = max_chunk_chars / 5; // ~20% overlap

    let build_breadcrumb = |stack: &[(usize, String)]| -> String {
        let mut parts: Vec<&str> = vec![file_path];
        for (_, h) in stack {
            parts.push(h.as_str());
        }
        parts.join(" > ")
    };

    let flush_section =
        |text: &str, offset: usize, breadcrumb: &str, out: &mut Vec<VaultChunk>| {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }

            if trimmed.len() <= max_chunk_chars {
                out.push(VaultChunk {
                    breadcrumb: breadcrumb.to_string(),
                    text: trimmed.to_string(),
                    char_offset: offset,
                    token_estimate: trimmed.len() / 4,
                });
            } else {
                // Split large sections with overlap
                let bytes = trimmed.as_bytes();
                let mut pos = 0;
                let mut chunk_idx = 0;
                while pos < bytes.len() {
                    let end = (pos + max_chunk_chars).min(bytes.len());
                    // Find a word boundary near `end`
                    let actual_end = if end < bytes.len() {
                        trimmed[pos..end]
                            .rfind(' ')
                            .map(|p| pos + p)
                            .unwrap_or(end)
                    } else {
                        end
                    };
                    let slice = &trimmed[pos..actual_end];
                    out.push(VaultChunk {
                        breadcrumb: if chunk_idx == 0 {
                            breadcrumb.to_string()
                        } else {
                            format!("{breadcrumb} (cont.)")
                        },
                        text: slice.to_string(),
                        char_offset: offset + pos,
                        token_estimate: slice.len() / 4,
                    });
                    chunk_idx += 1;

                    if actual_end >= bytes.len() {
                        break;
                    }
                    // Advance with overlap
                    let advance = if actual_end > pos + overlap_chars {
                        actual_end - pos - overlap_chars
                    } else {
                        actual_end - pos
                    };
                    pos += advance;
                }
            }
        };

    let mut offset = 0;
    for line in &lines {
        let line_len = line.len() + 1; // +1 for newline

        // Check if this is a heading
        if let Some(level) = heading_level(line) {
            let heading_text = line.trim_start_matches('#').trim().to_string();

            // Flush current section
            let breadcrumb = build_breadcrumb(&heading_stack);
            flush_section(&current_text, section_start, &breadcrumb, &mut chunks);
            current_text.clear();
            section_start = offset;

            // Update heading stack: pop headings at same or deeper level
            while heading_stack.last().is_some_and(|(l, _)| *l >= level) {
                heading_stack.pop();
            }
            heading_stack.push((level, heading_text));
        } else {
            current_text.push_str(line);
            current_text.push('\n');
        }

        offset += line_len;
    }

    // Flush final section
    let breadcrumb = build_breadcrumb(&heading_stack);
    flush_section(&current_text, section_start, &breadcrumb, &mut chunks);

    // If no chunks were created (no headings in the doc), make one from the whole body
    if chunks.is_empty() && !body.trim().is_empty() {
        flush_section(body, 0, file_path, &mut chunks);
    }

    chunks
}

fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    // Must be followed by a space
    if trimmed.len() > level && trimmed.as_bytes()[level] == b' ' {
        Some(level)
    } else {
        None
    }
}

/// Full index pass: walk the vault, parse every .md file, upsert brain_vault_nodes
/// + brain_vault_edges, chunk and write to rag_chunks. Incremental: only processes
/// files whose content_hash changed since last run.
pub async fn index_vault(pool: &PgPool, config: &VaultConfig) -> Result<IndexReport, String> {
    let brain_root = config.brain_root();
    if !brain_root.exists() {
        return Err(format!("Brain root does not exist: {}", brain_root.display()));
    }

    // Collect all .md files
    let md_files = collect_md_files(&brain_root)?;
    info!("Found {} .md files in vault", md_files.len());

    // Fetch existing hashes from DB for incremental indexing
    let existing_hashes = fetch_existing_hashes(pool).await?;

    let mut report = IndexReport {
        files_scanned: md_files.len(),
        nodes_upserted: 0,
        edges_created: 0,
        chunks_written: 0,
        unchanged_skipped: 0,
    };

    for file_path in &md_files {
        let node = parse_vault_file(file_path, &brain_root)?;

        // Check if content changed
        if let Some(old_hash) = existing_hashes.get(&node.path) {
            if *old_hash == node.content_hash {
                report.unchanged_skipped += 1;
                continue;
            }
        }

        upsert_node(pool, &node).await?;
        report.nodes_upserted += 1;

        // Upsert edges from wikilinks
        let edge_count = upsert_edges(pool, &node).await?;
        report.edges_created += edge_count;

        // Chunk and write
        let chunks = chunk_markdown(&node.body, &node.path);
        write_chunks(pool, &node.path, &chunks).await?;
        report.chunks_written += chunks.len();
    }

    info!(
        "Index complete: {} scanned, {} upserted, {} skipped, {} edges, {} chunks",
        report.files_scanned,
        report.nodes_upserted,
        report.unchanged_skipped,
        report.edges_created,
        report.chunks_written
    );

    Ok(report)
}

/// Incremental index: only re-process files in the given list (from git diff).
pub async fn index_changed_files(
    pool: &PgPool,
    config: &VaultConfig,
    paths: &[String],
) -> Result<IndexReport, String> {
    let brain_root = config.brain_root();
    let mut report = IndexReport {
        files_scanned: paths.len(),
        nodes_upserted: 0,
        edges_created: 0,
        chunks_written: 0,
        unchanged_skipped: 0,
    };

    for rel_path in paths {
        let full_path = brain_root.join(rel_path);
        if !full_path.exists() {
            debug!("Skipping deleted file: {rel_path}");
            // Mark node as invalid
            let _ = sqlx::query(
                "UPDATE brain_vault_nodes SET valid_until = NOW() WHERE path = $1 AND valid_until IS NULL",
            )
            .bind(rel_path)
            .execute(pool)
            .await
            .map_err(|e| format!("DB error invalidating node: {e}"))?;
            continue;
        }

        let node = parse_vault_file(&full_path, &brain_root)?;
        upsert_node(pool, &node).await?;
        report.nodes_upserted += 1;

        let edge_count = upsert_edges(pool, &node).await?;
        report.edges_created += edge_count;

        let chunks = chunk_markdown(&node.body, &node.path);
        write_chunks(pool, &node.path, &chunks).await?;
        report.chunks_written += chunks.len();
    }

    Ok(report)
}

// ── Internal helpers ──────────────────────────────────────────────────────

fn collect_md_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_md_recursive(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_md_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("Failed to read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Dir entry error: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_md_recursive(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

async fn fetch_existing_hashes(pool: &PgPool) -> Result<HashMap<String, String>, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT path, content_hash FROM brain_vault_nodes WHERE valid_until IS NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error fetching hashes: {e}"))?;

    Ok(rows.into_iter().collect())
}

async fn upsert_node(pool: &PgPool, node: &ParsedNode) -> Result<(), String> {
    sqlx::query(
        r#"
        INSERT INTO brain_vault_nodes (path, title, node_type, tags, extends_path,
                                       applies_to, from_thread, confidence, body,
                                       content_hash, indexed_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())
        ON CONFLICT (path) WHERE valid_until IS NULL
        DO UPDATE SET
            title = EXCLUDED.title,
            node_type = EXCLUDED.node_type,
            tags = EXCLUDED.tags,
            extends_path = EXCLUDED.extends_path,
            applies_to = EXCLUDED.applies_to,
            from_thread = EXCLUDED.from_thread,
            confidence = EXCLUDED.confidence,
            body = EXCLUDED.body,
            content_hash = EXCLUDED.content_hash,
            indexed_at = NOW()
        "#,
    )
    .bind(&node.path)
    .bind(&node.title)
    .bind(&node.node_type)
    .bind(&node.tags)
    .bind(&node.extends_path)
    .bind(&node.applies_to)
    .bind(&node.from_thread)
    .bind(node.confidence)
    .bind(&node.body)
    .bind(&node.content_hash)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error upserting node '{}': {e}", node.path))?;

    Ok(())
}

async fn upsert_edges(pool: &PgPool, node: &ParsedNode) -> Result<usize, String> {
    // Delete old edges for this source
    sqlx::query("DELETE FROM brain_vault_edges WHERE source_path = $1")
        .bind(&node.path)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error deleting edges: {e}"))?;

    let mut count = 0;

    // Wikilink edges
    for target in &node.wikilinks {
        sqlx::query(
            "INSERT INTO brain_vault_edges (source_path, target_path, edge_type) VALUES ($1, $2, 'link') ON CONFLICT DO NOTHING",
        )
        .bind(&node.path)
        .bind(target)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error inserting edge: {e}"))?;
        count += 1;
    }

    // Extends edge
    if let Some(extends) = &node.extends_path {
        sqlx::query(
            "INSERT INTO brain_vault_edges (source_path, target_path, edge_type) VALUES ($1, $2, 'extends') ON CONFLICT DO NOTHING",
        )
        .bind(&node.path)
        .bind(extends)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error inserting extends edge: {e}"))?;
        count += 1;
    }

    // Applies-to edges
    for target in &node.applies_to {
        sqlx::query(
            "INSERT INTO brain_vault_edges (source_path, target_path, edge_type) VALUES ($1, $2, 'applies_to') ON CONFLICT DO NOTHING",
        )
        .bind(&node.path)
        .bind(target)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error inserting applies_to edge: {e}"))?;
        count += 1;
    }

    Ok(count)
}

async fn write_chunks(pool: &PgPool, node_path: &str, chunks: &[VaultChunk]) -> Result<(), String> {
    // Delete old chunks for this node
    sqlx::query("DELETE FROM rag_chunks WHERE source_path = $1")
        .bind(node_path)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error deleting chunks: {e}"))?;

    for (i, chunk) in chunks.iter().enumerate() {
        sqlx::query(
            r#"
            INSERT INTO rag_chunks (source_path, chunk_index, breadcrumb, text,
                                    char_offset, token_estimate)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(node_path)
        .bind(i as i32)
        .bind(&chunk.breadcrumb)
        .bind(&chunk.text)
        .bind(chunk.char_offset as i64)
        .bind(chunk.token_estimate as i32)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error inserting chunk: {e}"))?;
    }

    Ok(())
}
