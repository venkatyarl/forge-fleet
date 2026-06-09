//! Cortex DATA lobe — native structured/tabular ingestion for the Brain faceted
//! graph. STEP 2 of multi-domain Cortex (code + docs + FINANCIALS + images).
//! This module is purely ADDITIVE: it does NOT touch the Rust code path in
//! `cortex.rs`, nor the document path in `doc_index.rs`. It walks a corpus root
//! for `.csv/.tsv` files and materializes them as graph nodes/edges, reusing the
//! exact same V117 Brain tables — mirroring `doc_index.rs` one-for-one.
//!
//! NODE MODEL (brain_vault_nodes)
//!   - `data:file`   — one per table. path `data://<slug>/<relpath>`,
//!                     title="<relpath>  (<N> rows)", project=slug,
//!                     content_hash=sha256(file contents).
//!   - `data:column` — one per header column. path
//!                     `data://<slug>/<relpath>#col=<colname>`, title=colname.
//!
//! EDGE MODEL (brain_vault_edges, provenance='cortex-data')
//!   - contains : file -> column (one per header field).
//!
//! FACETS
//!   Every inserted node is tagged with the corpus's modality=data facet
//!   (brain_node_facets), creating the facet row if missing (matching how
//!   corpus.rs / doc_index.rs seed facets via upsert semantics).
//!
//! IDEMPOTENT: like cortex::index() and doc_index::index_docs(), index_data()
//! DELETEs all prior `data:%` nodes for the corpus first (edges cascade via
//! ON DELETE CASCADE), then rebuilds.

use anyhow::Result;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Summary of a data-indexing run.
#[derive(Debug, Default, Clone)]
pub struct DataStats {
    pub files: usize,
    pub columns: usize,
    pub rows: usize,
    pub edges: usize,
}

/// Directory names we never descend into.
fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | "build" | ".forgefleet"
    )
}

/// Field delimiter for a given extension (`,` for csv, `\t` for tsv).
fn delim_for_ext(ext: &str) -> Option<char> {
    match ext {
        "csv" => Some(','),
        "tsv" => Some('\t'),
        _ => None,
    }
}

/// Index a corpus's structured data files into the Brain faceted graph.
///
/// Re-uses the cached `PgPool` (passed in). Walks `root` for `.csv/.tsv` files
/// and writes `data:file` / `data:column` nodes + `contains` edges + the
/// modality=data facet.
pub async fn index_data(pool: &PgPool, corpus_slug: &str, root: &Path) -> Result<DataStats> {
    // Resolve corpus id (also serves as a guard that the corpus exists).
    let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
        .bind(corpus_slug)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

    // Idempotent: drop all prior data:* nodes for this corpus (edges cascade).
    sqlx::query(
        "DELETE FROM brain_vault_nodes
           WHERE project = $1 AND node_type LIKE 'data:%'",
    )
    .bind(corpus_slug)
    .execute(pool)
    .await?;

    // Ensure the modality=data facet exists; remember its id for tagging.
    let data_facet_id = upsert_modality_data_facet(pool, corpus_id).await?;

    let mut stats = DataStats::default();

    let files = collect_data_files(root);
    for file_path in files {
        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(_) => continue, // unreadable / vanished — skip
        };
        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let delim = match delim_for_ext(&ext) {
            Some(d) => d,
            None => continue,
        };
        let rel = file_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| file_path.to_string_lossy().to_string());

        // First non-empty line is the header; everything after is a data row.
        let (header, row_count) = parse_table(&source, delim);
        if header.is_empty() {
            continue; // no header → not a usable table
        }

        // ── file node ───────────────────────────────────────────────────────
        let file_node_path = format!("data://{corpus_slug}/{rel}");
        let file_title = format!("{rel}  ({row_count} rows)");
        let content_hash = sha256_hex(&source);
        let file_id = upsert_data_node(
            pool,
            &file_node_path,
            &file_title,
            "data:file",
            corpus_slug,
            &content_hash,
        )
        .await?;
        stats.files += 1;
        stats.rows += row_count;
        tag_facet(pool, corpus_id, file_id, data_facet_id).await?;

        // ── column nodes + contains edges ───────────────────────────────────
        for col in &header {
            let col_path = format!("data://{corpus_slug}/{rel}#col={col}");
            let col_id = upsert_data_node(
                pool,
                &col_path,
                col,
                "data:column",
                corpus_slug,
                &sha256_hex(&format!("{rel}#col={col}")),
            )
            .await?;
            stats.columns += 1;
            tag_facet(pool, corpus_id, col_id, data_facet_id).await?;
            if add_data_edge(pool, file_id, col_id, "contains").await? {
                stats.edges += 1;
            }
        }
    }

    Ok(stats)
}

// ─── DB helpers ──────────────────────────────────────────────────────────────

/// Upsert a data node by its synthetic unique `path`. Mirrors doc_index's
/// upsert_doc_node.
async fn upsert_data_node(
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
async fn add_data_edge(pool: &PgPool, src: Uuid, dst: Uuid, edge_type: &str) -> Result<bool> {
    let r = sqlx::query(
        r#"INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, provenance)
           VALUES ($1, $2, $3, 'cortex-data')
           ON CONFLICT (src_id, dst_id, edge_type) DO NOTHING"#,
    )
    .bind(src)
    .bind(dst)
    .bind(edge_type)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Ensure the corpus's modality=data facet exists; return its id. Matches the
/// SEED_FACETS row (`modality`, `data`, `Data`) and corpus.rs's upsert_facet.
async fn upsert_modality_data_facet(pool: &PgPool, corpus_id: Uuid) -> Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_facets (corpus_id, dimension, value, title)
           VALUES ($1, 'modality', 'data', 'Data')
           ON CONFLICT (corpus_id, dimension, value) DO UPDATE
             SET title = COALESCE(EXCLUDED.title, brain_facets.title)
           RETURNING id"#,
    )
    .bind(corpus_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Tag a node with the modality=data facet (idempotent).
async fn tag_facet(pool: &PgPool, corpus_id: Uuid, node_id: Uuid, facet_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO brain_node_facets (corpus_id, node_id, node_kind, facet_id, provenance)
           VALUES ($1, $2, 'content', $3, 'cortex-data')
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

/// Recursively collect `.csv/.tsv` files under `root`, skipping vendored dirs.
fn collect_data_files(root: &Path) -> Vec<PathBuf> {
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
                    if delim_for_ext(&ext.to_lowercase()).is_some() {
                        out.push(path);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

// ─── delimited-table parsing ─────────────────────────────────────────────────

/// Parse a delimited file: return (header fields, data-row count). The first
/// non-empty line is the header; subsequent non-empty lines are data rows.
/// Quoting: a field wrapped in double-quotes may contain the delimiter and
/// embedded `""` (an escaped quote); newlines inside quotes are not supported
/// (each physical line is treated as one record — adequate for header detection
/// and row counting on well-formed financial exports).
fn parse_table(source: &str, delim: char) -> (Vec<String>, usize) {
    let mut header: Vec<String> = Vec::new();
    let mut rows = 0usize;
    for line in source.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if header.is_empty() {
            header = split_delimited(line, delim);
        } else {
            rows += 1;
        }
    }
    (header, rows)
}

/// Split one line on `delim`, honoring simple double-quote quoting. A field that
/// starts with `"` is read until the closing `"`; `""` inside becomes a literal
/// `"`. Surrounding quotes are stripped from the emitted field.
fn split_delimited(line: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    // Escaped quote ("") → literal ".
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.is_empty() {
            in_quotes = true;
        } else if c == delim {
            out.push(field.trim().to_string());
            field = String::new();
        } else {
            field.push(c);
        }
    }
    out.push(field.trim().to_string());
    out
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv_header_and_counts_rows() {
        let src = "id,name,amount\n1,Acme,100\n2,Globex,200\n3,Initech,300\n";
        let (header, rows) = parse_table(src, ',');
        assert_eq!(header, vec!["id", "name", "amount"]);
        assert_eq!(rows, 3);
    }

    #[test]
    fn header_honors_quoted_fields_with_embedded_delimiter() {
        // A quoted header field contains a comma and an escaped quote.
        let line = r#"id,"last, first","note ""x""",amount"#;
        let header = split_delimited(line, ',');
        assert_eq!(header, vec!["id", "last, first", "note \"x\"", "amount"]);
    }

    #[test]
    fn tsv_splits_on_tab_and_skips_blank_lines() {
        let src = "ticker\tprice\tqty\n\nAAPL\t190\t10\n\nMSFT\t420\t5\n";
        let (header, rows) = parse_table(src, '\t');
        assert_eq!(header, vec!["ticker", "price", "qty"]);
        assert_eq!(rows, 2);
    }
}
