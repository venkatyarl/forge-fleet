//! Cortex DOCUMENTS lobe — native Markdown/plaintext ingestion for the Brain
//! faceted graph. STEP 1 of multi-domain Cortex (code + docs + financials +
//! images). This module is purely ADDITIVE: it does NOT touch the Rust code path
//! in `cortex.rs`. It walks a corpus root for `.md/.mdx/.markdown/.txt` files and
//! materializes them as graph nodes/edges, reusing the exact V117 Brain tables.
//!
//! NODE MODEL (brain_vault_nodes)
//!   - `doc:file`    — one per document. path `doc://<slug>/<relpath>`,
//!                     title=relpath, project=slug. content_hash is a metadata
//!                     (size+mtime) cheap_hash — the change signal mirrors the
//!                     cortex corpus scan, so an unchanged doc is skipped WITHOUT
//!                     reading the file (images stay content-hashed — see
//!                     image_index.rs for why).
//!   - `doc:section` — one per markdown heading (`#`..`######`). path
//!                     `doc://<slug>/<relpath>#<anchor>`, title=heading text.
//!                     (`.txt` files have no headings → file node only.)
//!
//! EDGE MODEL (brain_vault_edges, provenance='cortex-doc')
//!   - contains : file -> top-level section, AND section -> subsection nested by
//!                heading depth (a level-3 hangs off the nearest preceding level-2,
//!                etc). A section with no shallower ancestor hangs off the file.
//!
//! FACETS
//!   Every inserted node is tagged with the corpus's modality=doc facet
//!   (brain_node_facets), creating the facet row if missing (matching how
//!   corpus.rs seeds facets via upsert_facet semantics).
//!
//! FULL vs INCREMENTAL: a full reindex DELETEs all prior `doc:%` nodes for the
//! corpus first (edges cascade via ON DELETE CASCADE), then rebuilds — re-parsing
//! every file. An incremental run keeps existing nodes and re-parses ONLY files
//! whose size+mtime changed since the last index (matched by the metadata
//! `content_hash`), rebuilding just the changed file's sections; it then GCs the
//! `doc:file` + `doc:section` nodes of files removed on disk. On a large doc tree
//! (forge-fleet: 2833 files / 36k sections) skipping unchanged files takes a no-op
//! `--incremental` run from ~100s of DB churn to a metadata-only stat per file —
//! no content read, no sha256.

use anyhow::Result;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Summary of a documents-indexing run.
#[derive(Debug, Default, Clone)]
pub struct DocStats {
    pub files: usize,
    pub sections: usize,
    pub edges: usize,
}

/// Directory names we never descend into.
fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | "build" | ".forgefleet"
    )
}

/// Extensions we treat as documents.
fn is_doc_ext(ext: &str) -> bool {
    matches!(ext, "md" | "mdx" | "markdown" | "txt")
}

/// A parsed markdown heading.
struct Heading {
    /// 1..=6 (number of leading `#`).
    level: usize,
    /// The heading text (after the `#`s, trimmed).
    text: String,
}

/// Index a corpus's document files into the Brain faceted graph.
///
/// Re-uses the cached `PgPool` (passed in). Walks `root` for doc files and writes
/// `doc:file` / `doc:section` nodes + `contains` edges + the modality=doc facet.
pub async fn index_docs(
    pool: &PgPool,
    corpus_slug: &str,
    root: &Path,
    incremental: bool,
) -> Result<DocStats> {
    // Resolve corpus id (also serves as a guard that the corpus exists).
    let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
        .bind(corpus_slug)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

    // Full: drop all prior doc:* nodes (edges cascade), rebuild from scratch.
    // Incremental: keep them and load each doc:file's last-indexed content_hash
    // so an unchanged file can skip the re-parse below.
    let prior_hashes: HashMap<String, String> = if incremental {
        sqlx::query(
            "SELECT path, content_hash FROM brain_vault_nodes
               WHERE project = $1 AND node_type = 'doc:file'",
        )
        .bind(corpus_slug)
        .fetch_all(pool)
        .await?
        .into_iter()
        .filter_map(|r| {
            let path: String = r.get("path");
            r.get::<Option<String>, _>("content_hash")
                .map(|h| (path, h))
        })
        .collect()
    } else {
        sqlx::query(
            "DELETE FROM brain_vault_nodes
               WHERE project = $1 AND node_type LIKE 'doc:%'",
        )
        .bind(corpus_slug)
        .execute(pool)
        .await?;
        HashMap::new()
    };

    // Ensure the modality=doc facet exists; remember its id for tagging.
    let doc_facet_id = upsert_modality_doc_facet(pool, corpus_id).await?;

    let mut stats = DocStats::default();

    // File-node paths present on disk this run — used to GC removed docs.
    let files = collect_doc_files(root);
    let mut live_paths: Vec<String> = Vec::with_capacity(files.len());
    for file_path in files {
        let rel = file_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| file_path.to_string_lossy().to_string());

        // ── file node ───────────────────────────────────────────────────────
        let file_node_path = format!("doc://{corpus_slug}/{rel}");

        // Change signal is a metadata-only hash (size+mtime), mirroring the
        // cortex corpus scan — an unchanged doc is detected WITHOUT reading the
        // file, so a no-op incremental run skips the read + re-parse entirely
        // (the read+sha256 of every doc was the residual no-op cost).
        let Ok(md) = std::fs::metadata(&file_path) else {
            continue; // vanished — the GC below drops its now-absent node
        };
        live_paths.push(file_node_path.clone());
        let content_hash =
            crate::corpus::cheap_hash(&file_node_path, md.len(), crate::corpus::mtime_of(&md));

        // Incremental: an unchanged file keeps its node + sections + edges —
        // skip the read + re-parse entirely.
        if incremental
            && doc_unchanged(
                prior_hashes.get(&file_node_path).map(String::as_str),
                &content_hash,
            )
        {
            stats.files += 1;
            continue;
        }

        // Changed (or full run): read the content to parse headings/sections.
        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(_) => continue, // unreadable / vanished — skip
        };

        let file_id = upsert_doc_node(
            pool,
            &file_node_path,
            &rel,
            "doc:file",
            corpus_slug,
            &content_hash,
        )
        .await?;
        stats.files += 1;
        tag_facet(pool, corpus_id, file_id, doc_facet_id).await?;

        // A changed file keeps its node id (stable path) but its prior sections
        // are now stale (a heading may have been renamed/removed). Drop them by
        // their `<file>#` path prefix before rebuilding — the `#` makes the
        // prefix unambiguous (it can't match another file's sections). A full
        // run already wiped everything, so this is a no-op there.
        if incremental {
            sqlx::query(
                "DELETE FROM brain_vault_nodes
                   WHERE project = $1 AND node_type = 'doc:section'
                     AND starts_with(path, $2)",
            )
            .bind(corpus_slug)
            .bind(format!("{file_node_path}#"))
            .execute(pool)
            .await?;
        }

        // ── section nodes + contains edges ──────────────────────────────────
        // Parse headings; build the depth-nesting (a level-N section's parent is
        // the nearest preceding section with a strictly smaller level, else file).
        // `stack` holds (level, node_id) of currently-open ancestor sections.
        let mut stack: Vec<(usize, Uuid)> = Vec::new();
        for h in parse_headings(&source) {
            let anchor = anchor_for(&h.text);
            let sec_path = format!("doc://{corpus_slug}/{rel}#{anchor}");
            let sec_id = upsert_doc_node(
                pool,
                &sec_path,
                &h.text,
                "doc:section",
                corpus_slug,
                &sha256_hex(&format!("{rel}#{anchor}")),
            )
            .await?;
            stats.sections += 1;
            tag_facet(pool, corpus_id, sec_id, doc_facet_id).await?;

            // Pop ancestors at >= this heading's level.
            while let Some(&(lvl, _)) = stack.last() {
                if lvl >= h.level {
                    stack.pop();
                } else {
                    break;
                }
            }
            // Parent = nearest shallower section on the stack, else the file.
            let parent_id = stack.last().map(|&(_, id)| id).unwrap_or(file_id);
            if add_doc_edge(pool, parent_id, sec_id, "contains").await? {
                stats.edges += 1;
            }
            stack.push((h.level, sec_id));
        }
    }

    // Incremental GC: drop doc:file nodes whose file vanished on disk, plus every
    // doc:section whose owning file (the path before the first `#`) is no longer
    // live. `split_part(path,'#',1)` recovers a section's file path regardless of
    // heading nesting, so this catches sub-sections too. `x <> ALL('{}')` is
    // vacuously true, so an empty corpus correctly drops every doc node.
    if incremental {
        sqlx::query(
            "DELETE FROM brain_vault_nodes
               WHERE project = $1 AND node_type = 'doc:file'
                 AND path <> ALL($2)",
        )
        .bind(corpus_slug)
        .bind(&live_paths)
        .execute(pool)
        .await?;
        sqlx::query(
            "DELETE FROM brain_vault_nodes
               WHERE project = $1 AND node_type = 'doc:section'
                 AND split_part(path, '#', 1) <> ALL($2)",
        )
        .bind(corpus_slug)
        .bind(&live_paths)
        .execute(pool)
        .await?;
    }

    Ok(stats)
}

// ─── DB helpers ──────────────────────────────────────────────────────────────

/// Upsert a doc node by its synthetic unique `path`. Mirrors cortex's
/// upsert_code_node but carries a real content_hash for `doc:file`.
async fn upsert_doc_node(
    pool: &PgPool,
    path: &str,
    title: &str,
    node_type: &str,
    project: &str,
    content_hash: &str,
) -> Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_vault_nodes (path, title, node_type, project, content_hash)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (path) DO UPDATE
             SET title = EXCLUDED.title, node_type = EXCLUDED.node_type,
                 project = EXCLUDED.project, content_hash = EXCLUDED.content_hash,
                 valid_until = NULL, updated_at = NOW()
           RETURNING id"#,
    )
    .bind(path)
    .bind(title)
    .bind(node_type)
    .bind(project)
    .bind(content_hash)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Returns true if a new edge row was inserted (false if it already existed).
async fn add_doc_edge(pool: &PgPool, src: Uuid, dst: Uuid, edge_type: &str) -> Result<bool> {
    let r = sqlx::query(
        r#"INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, provenance)
           VALUES ($1, $2, $3, 'cortex-doc')
           ON CONFLICT (src_id, dst_id, edge_type) DO NOTHING"#,
    )
    .bind(src)
    .bind(dst)
    .bind(edge_type)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Ensure the corpus's modality=doc facet exists; return its id. Matches the
/// SEED_FACETS row (`modality`, `doc`, `Doc`) and corpus.rs's upsert_facet.
async fn upsert_modality_doc_facet(pool: &PgPool, corpus_id: Uuid) -> Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_facets (corpus_id, dimension, value, title)
           VALUES ($1, 'modality', 'doc', 'Doc')
           ON CONFLICT (corpus_id, dimension, value) DO UPDATE
             SET title = COALESCE(EXCLUDED.title, brain_facets.title)
           RETURNING id"#,
    )
    .bind(corpus_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Tag a node with the modality=doc facet (idempotent).
async fn tag_facet(pool: &PgPool, corpus_id: Uuid, node_id: Uuid, facet_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO brain_node_facets (corpus_id, node_id, node_kind, facet_id, provenance)
           VALUES ($1, $2, 'content', $3, 'cortex-doc')
           ON CONFLICT (node_id, facet_id) DO NOTHING"#,
    )
    .bind(corpus_id)
    .bind(node_id)
    .bind(facet_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── filesystem walk ─────────────────────────────────────────────────────────

/// Recursively collect document files under `root`, skipping heavy/vendored dirs.
fn collect_doc_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if visited > 100_000 {
            break;
        }
        visited += 1;
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                if !skip_dir(&name) {
                    stack.push(path);
                }
            } else if ft.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if is_doc_ext(&ext.to_lowercase()) {
                        out.push(path);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

// ─── markdown parsing ────────────────────────────────────────────────────────

/// Extract markdown ATX headings (`#`..`######`) from a document. Lines inside
/// fenced code blocks (``` / ~~~) are ignored so `# comment` in a snippet isn't
/// mistaken for a heading.
fn parse_headings(source: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = "";
    for line in source.lines() {
        let trimmed = line.trim_start();
        // Toggle fenced code blocks.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = if trimmed.starts_with("```") {
                "```"
            } else {
                "~~~"
            };
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if marker == fence_marker {
                in_fence = false;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(h) = parse_heading_line(line) {
            out.push(h);
        }
    }
    out
}

/// Parse one line as an ATX heading, or None. `### Title` → level 3, "Title".
fn parse_heading_line(line: &str) -> Option<Heading> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &t[hashes..];
    // Must be `#` then a space (or end) to be a heading, not `#foo`.
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    // Strip optional trailing `###` (closed ATX) and surrounding whitespace.
    let text = rest.trim().trim_end_matches('#').trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Heading {
        level: hashes,
        text,
    })
}

/// Slugify a heading into a GitHub-style anchor (lowercase, alnum + dashes).
fn anchor_for(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "section".to_string()
    } else {
        s
    }
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// On an incremental run, a doc file can skip the re-parse only when we've
/// indexed this exact path before with the identical content hash. A new file
/// (no prior node) or one whose content changed must be re-parsed.
fn doc_unchanged(prior_hash: Option<&str>, current_hash: &str) -> bool {
    prior_hash == Some(current_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_unchanged_only_when_hash_matches() {
        // Same path, identical content → skip the re-parse.
        assert!(doc_unchanged(Some("abc"), "abc"));
        // Same path, content changed → must re-parse.
        assert!(!doc_unchanged(Some("abc"), "def"));
        // New file (no prior node) → must parse.
        assert!(!doc_unchanged(None, "abc"));
    }

    #[test]
    fn parses_atx_headings_with_depth() {
        let src = "# Top\nbody\n## Sub\n### Deep\ntext\n## Sub2\n";
        let hs = parse_headings(src);
        let pairs: Vec<(usize, &str)> = hs.iter().map(|h| (h.level, h.text.as_str())).collect();
        assert_eq!(
            pairs,
            vec![(1, "Top"), (2, "Sub"), (3, "Deep"), (2, "Sub2")]
        );
    }

    #[test]
    fn ignores_headings_inside_code_fences() {
        let src = "# Real\n```\n# not a heading\n```\n## After\n";
        let hs = parse_headings(src);
        let texts: Vec<&str> = hs.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Real", "After"]);
    }

    #[test]
    fn non_heading_hash_is_skipped() {
        assert!(parse_heading_line("#nospace").is_none());
        assert!(parse_heading_line("####### too many").is_none());
        assert!(parse_heading_line("not a heading").is_none());
        assert_eq!(parse_heading_line("## Closed ##").unwrap().text, "Closed");
    }

    #[test]
    fn anchor_slugifies() {
        assert_eq!(anchor_for("Hello, World!"), "hello-world");
        assert_eq!(anchor_for("  Multi   Space  "), "multi-space");
        assert_eq!(anchor_for("!!!"), "section");
    }
}
