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

const MAX_FILE_SIZE: u64 = 500_000; // 500KB — skip huge generated/API-dump files

/// Parse a single .md file into a ParsedNode.
pub fn parse_vault_file(path: &Path, vault_root: &Path) -> Result<ParsedNode, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("metadata {}: {e}", path.display()))?;
    if meta.len() > MAX_FILE_SIZE {
        return Err(format!("skipping oversized file ({} bytes): {}", meta.len(), path.display()));
    }
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
                // Split large sections with overlap — char-boundary safe.
                let chars: Vec<char> = trimmed.chars().collect();
                let total = chars.len();
                let chunk_chars = max_chunk_chars / 4; // work in char count not byte count
                let overlap = chunk_chars / 5;
                let mut pos = 0;
                let mut chunk_idx = 0;
                while pos < total {
                    let end = (pos + chunk_chars).min(total);
                    let actual_end = if end < total {
                        // Find space near end
                        let window: String = chars[pos..end].iter().collect();
                        match window.rfind(' ') {
                            Some(sp) => pos + sp,
                            None => end,
                        }
                    } else {
                        end
                    };
                    let slice: String = chars[pos..actual_end].iter().collect();
                    let byte_offset = trimmed.chars().take(pos).map(|c| c.len_utf8()).sum::<usize>();
                    out.push(VaultChunk {
                        breadcrumb: if chunk_idx == 0 {
                            breadcrumb.to_string()
                        } else {
                            format!("{breadcrumb} (cont.)")
                        },
                        text: slice.clone(),
                        char_offset: offset + byte_offset,
                        token_estimate: slice.len() / 4,
                    });
                    chunk_idx += 1;

                    if actual_end >= total { break; }
                    let advance = if actual_end > pos + overlap {
                        actual_end - pos - overlap
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
        let node = match parse_vault_file(file_path, &brain_root) {
            Ok(n) => n,
            Err(e) => {
                if e.contains("skipping oversized") {
                    debug!("{e}");
                } else {
                    warn!("parse error: {e}");
                }
                report.unchanged_skipped += 1;
                continue;
            }
        };

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
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
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
    // Use the pg_upsert_brain_vault_node helper from ff-db (matches V13 schema).
    ff_db::pg_upsert_brain_vault_node(
        pool,
        &node.path,
        &node.title,
        node.node_type.as_deref(),
        None, // project — derived from folder path later
        &node.tags,
        node.extends_path.as_deref(),
        &node.applies_to,
        node.from_thread.as_deref(),
        node.confidence,
        &node.content_hash,
    )
    .await
    .map_err(|e| format!("DB error upserting node '{}': {e}", node.path))?;
    Ok(())
}

async fn upsert_edges(pool: &PgPool, node: &ParsedNode) -> Result<usize, String> {
    // Resolve source node UUID.
    let src = ff_db::pg_get_brain_vault_node(pool, &node.path)
        .await
        .map_err(|e| format!("get src node: {e}"))?;
    let src_id = match src {
        Some(n) => n.id,
        None => return Ok(0),
    };

    let mut count = 0;

    // Wikilink edges — resolve target by matching the wikilink text to an
    // existing node path (basename match, Obsidian-style shortest path).
    for target in &node.wikilinks {
        if let Some(dst) = resolve_wikilink_target(pool, target).await {
            let _ = ff_db::pg_upsert_brain_vault_edge(
                pool, src_id, dst, "link", 1.0, "extracted",
            ).await;
            count += 1;
        }
    }

    // Extends edge
    if let Some(extends) = &node.extends_path {
        let clean = extends.trim_start_matches("[[").trim_end_matches("]]");
        if let Some(dst) = resolve_wikilink_target(pool, clean).await {
            let _ = ff_db::pg_upsert_brain_vault_edge(
                pool, src_id, dst, "extends", 1.0, "extracted",
            ).await;
            count += 1;
        }
    }

    // Applies-to edges
    for target in &node.applies_to {
        let clean = target.trim_start_matches("[[").trim_end_matches("]]");
        if let Some(dst) = resolve_wikilink_target(pool, clean).await {
            let _ = ff_db::pg_upsert_brain_vault_edge(
                pool, src_id, dst, "applies_to", 1.0, "extracted",
            ).await;
            count += 1;
        }
    }

    Ok(count)
}

/// Resolve a wikilink target text (e.g. "UI Design" or "Projects/ForgeFleet/UI Design")
/// to an existing brain_vault_nodes.id. Uses Obsidian's shortest-path semantics:
/// first try exact path match, then basename match.
async fn resolve_wikilink_target(pool: &PgPool, target: &str) -> Option<uuid::Uuid> {
    // Try exact path match (e.g. "Projects/ForgeFleet/UI Design.md")
    let with_md = if target.ends_with(".md") { target.to_string() } else { format!("{target}.md") };
    if let Ok(Some(node)) = ff_db::pg_get_brain_vault_node(pool, &with_md).await {
        return Some(node.id);
    }
    // Try basename match (e.g. "UI Design" matches "any/path/UI Design.md")
    let basename = target.rsplit('/').next().unwrap_or(target);
    let pattern = format!("%/{basename}.md");
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT id FROM brain_vault_nodes WHERE path LIKE $1 AND valid_until IS NULL LIMIT 1",
    )
    .bind(&pattern)
    .fetch_optional(pool)
    .await
    .ok()?;
    row.map(|r| r.0)
}

async fn write_chunks(pool: &PgPool, node_path: &str, chunks: &[VaultChunk]) -> Result<(), String> {
    // The rag_chunks table is created by ff-memory's RAG engine, not by our
    // V13 migration. If it doesn't exist, skip chunk writing silently — nodes
    // + edges still index fine without chunks. Chunks enable semantic search
    // which only kicks in once pgvector + embeddings are deployed (Phase 4b).
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = 'rag_chunks')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false);
    if !table_exists {
        return Ok(());
    }

    let _ = sqlx::query("DELETE FROM rag_chunks WHERE workspace_id = 'brain_vault' AND source_path = $1")
        .bind(node_path)
        .execute(pool)
        .await;

    // Deterministic document_id from path (simple hash-based).
    let mut hasher = Sha256::new();
    hasher.update(b"brain_vault_doc:");
    hasher.update(node_path.as_bytes());
    let hash = hasher.finalize();
    let doc_id = uuid::Uuid::from_slice(&hash[..16]).unwrap_or_else(|_| uuid::Uuid::new_v4());

    for (i, chunk) in chunks.iter().enumerate() {
        let chunk_id = uuid::Uuid::new_v4();
        let metadata = serde_json::json!({
            "breadcrumb": chunk.breadcrumb,
            "char_offset": chunk.char_offset,
            "token_estimate": chunk.token_estimate,
        });
        let _ = sqlx::query(
            "INSERT INTO rag_chunks (id, workspace_id, document_id, source_path, chunk_index, content, metadata)
             VALUES ($1, 'brain_vault', $2, $3, $4, $5, $6)",
        )
        .bind(chunk_id)
        .bind(doc_id)
        .bind(node_path)
        .bind(i as i32)
        .bind(&chunk.text)
        .bind(metadata.to_string())
        .execute(pool)
        .await
        .map_err(|e| format!("DB error inserting chunk: {e}"))?;
    }

    Ok(())
}
