//! Cortex IMAGES lobe — native image ingestion (+ best-effort VISION captioning)
//! for the Brain faceted graph. STEP 3 of multi-domain Cortex (code + docs +
//! financials + IMAGES). This module is purely ADDITIVE: it does NOT touch the
//! Rust code path in `cortex.rs`, the document path in `doc_index.rs`, nor the
//! tabular path in `data_index.rs`. It walks a corpus root for image files and
//! materializes them as graph nodes/edges, reusing the exact same V117 Brain
//! tables — mirroring `data_index.rs` one-for-one, plus a bounded, best-effort
//! vision-LLM caption/tag pass.
//!
//! NODE MODEL (brain_vault_nodes)
//!   - `image:file` — one per image. path `image://<slug>/<relpath>`,
//!                    title=relpath (or the vision caption when captioning
//!                    succeeds), project=slug, content_hash=sha256(file bytes),
//!                    valid_until NULL.
//!   - `image:tag`  — one per distinct lowercase tag emitted by the vision pass.
//!                    path `image://<slug>/tag=<tag>`, title=tag.
//!
//! EDGE MODEL (brain_vault_edges, provenance='cortex-image')
//!   - tagged : image:file -> image:tag (one per emitted tag).
//!
//! VISION (best-effort)
//!   For up to `MAX_CAPTION` images we base64-encode the bytes and POST a
//!   data-URL to the fleet's healthy vision endpoint (qwen3-vl-30b). On success
//!   we use the returned caption as the file node's title and create `image:tag`
//!   nodes. On ANY error/timeout we keep the bare `image:file` node and move on —
//!   captioning never fails the index.
//!
//! FACETS
//!   Every inserted node is tagged with the corpus's modality=image facet
//!   (brain_node_facets), creating the facet row if missing (matching how
//!   corpus.rs / data_index.rs seed facets via upsert semantics).
//!
//! FULL vs INCREMENTAL: a full reindex DELETEs all prior `image:%` nodes for the
//! corpus first (edges cascade via ON DELETE CASCADE), then rebuilds — re-running
//! the vision caption pass on every image. An incremental run keeps the existing
//! nodes and re-captions ONLY images whose bytes changed since the last index
//! (matched by `content_hash`), GCs nodes for images removed on disk, and sweeps
//! orphaned `image:tag` nodes. Since the vision pass (one LLM call per image) is
//! the dominant cost of `ff cortex index`, skipping unchanged images takes a
//! no-op `--incremental` run from minutes to ~instant.

use anyhow::Result;
use base64::Engine;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Fleet vision endpoint (healthy qwen3-vl-30b on James, 192.168.5.108).
const VISION_URL: &str = "http://192.168.5.108:55003/v1/chat/completions";
const VISION_MODEL: &str = "qwen3-vl-30b-a3b";
/// Cap on how many images we run through the vision pass per index, to keep the
/// pass bounded regardless of corpus size.
const MAX_CAPTION: usize = 25;
/// Skip images larger than ~8 MB (too big to base64 into a single request).
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Per-image vision request timeout.
const VISION_TIMEOUT_SECS: u64 = 20;

/// Summary of an image-indexing run.
#[derive(Debug, Default, Clone)]
pub struct ImageStats {
    pub files: usize,
    pub captioned: usize,
    pub tags: usize,
    pub edges: usize,
}

/// Directory names we never descend into.
fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | "build" | ".forgefleet"
    )
}

/// Extensions we treat as images.
fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
}

/// MIME `image/<subtype>` for a (lowercased) extension. Used to build the
/// data-URL prefix for the vision request.
fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// Index a corpus's image files into the Brain faceted graph.
///
/// Re-uses the cached `PgPool` (passed in). Walks `root` for image files and
/// writes `image:file` nodes + (best-effort) `image:tag` nodes + `tagged` edges
/// + the modality=image facet.
///
/// `incremental`: when false (full reindex) every prior `image:%` node is wiped
/// and every image is re-captioned. When true, existing nodes are kept and only
/// images whose `content_hash` changed since the last index are re-captioned;
/// nodes for images removed on disk are GC'd, as are orphaned `image:tag` nodes.
/// The vision pass is the dominant cost of an index run (one LLM call per image),
/// so the incremental path makes a no-op run effectively free.
pub async fn index_images(
    pool: &PgPool,
    corpus_slug: &str,
    root: &Path,
    incremental: bool,
) -> Result<ImageStats> {
    // Resolve corpus id (also serves as a guard that the corpus exists).
    let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
        .bind(corpus_slug)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

    // Full: drop all prior image:* nodes (edges cascade), rebuild from scratch.
    // Incremental: keep them and load each image:file's last-indexed content_hash
    // so an unchanged image can skip the expensive vision caption call below.
    let prior_hashes: HashMap<String, String> = if incremental {
        sqlx::query(
            "SELECT path, content_hash FROM brain_vault_nodes
               WHERE project = $1 AND node_type = 'image:file'",
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
               WHERE project = $1 AND node_type LIKE 'image:%'",
        )
        .bind(corpus_slug)
        .execute(pool)
        .await?;
        HashMap::new()
    };

    // Ensure the modality=image facet exists; remember its id for tagging.
    let image_facet_id = upsert_modality_image_facet(pool, corpus_id).await?;

    let mut stats = ImageStats::default();

    let files = collect_image_files(root);
    let mut caption_budget = MAX_CAPTION;
    if files.len() > MAX_CAPTION {
        tracing::info!(
            "image_index: {} images found; vision caption pass capped at {}",
            files.len(),
            MAX_CAPTION
        );
    }

    // Build one HTTP client for the whole pass (cached, not per-call).
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(VISION_TIMEOUT_SECS))
        .build()
        .ok();

    // File-node paths present on disk this run — used to GC removed images.
    let mut live_paths: Vec<String> = Vec::with_capacity(files.len());

    for file_path in files {
        let bytes = match std::fs::read(&file_path) {
            Ok(b) => b,
            Err(_) => continue, // unreadable / vanished — skip
        };
        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let rel = file_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| file_path.to_string_lossy().to_string());

        let file_node_path = format!("image://{corpus_slug}/{rel}");
        let content_hash = sha256_hex_bytes(&bytes);
        live_paths.push(file_node_path.clone());

        // Incremental: an unchanged image keeps its existing node, caption and
        // tags — skip the expensive vision call (the dominant cost) entirely.
        if incremental
            && image_unchanged(
                prior_hashes.get(&file_node_path).map(String::as_str),
                &content_hash,
            )
        {
            stats.files += 1;
            continue;
        }

        // ── best-effort vision caption ──────────────────────────────────────
        // Default title is the relpath; if captioning succeeds we use the
        // caption instead and emit image:tag nodes.
        let mut title = rel.clone();
        let mut caption_tags: Vec<String> = Vec::new();
        let mut did_caption = false;
        if caption_budget > 0 {
            if let Some(client) = http.as_ref() {
                match caption_image(client, &bytes, &ext).await {
                    Ok((caption, tags)) => {
                        if !caption.is_empty() {
                            title = caption;
                        }
                        caption_tags = tags;
                        did_caption = true;
                    }
                    Err(e) => {
                        tracing::debug!("image_index: caption failed for {rel}: {e}");
                    }
                }
            }
            caption_budget -= 1;
        }

        // ── file node ───────────────────────────────────────────────────────
        let file_id = upsert_image_node(
            pool,
            &file_node_path,
            &title,
            "image:file",
            corpus_slug,
            &content_hash,
        )
        .await?;
        stats.files += 1;
        if did_caption {
            stats.captioned += 1;
        }
        tag_facet(pool, corpus_id, file_id, image_facet_id).await?;

        // A changed image keeps its node id (stable path) but its prior caption's
        // `tagged` edges are now stale — clear them before re-tagging so the new
        // caption's tags fully replace the old set (orphaned tag nodes are GC'd
        // after the loop). A full run already wiped everything, so this is a
        // no-op there.
        if incremental {
            sqlx::query("DELETE FROM brain_vault_edges WHERE src_id = $1 AND edge_type = 'tagged'")
                .bind(file_id)
                .execute(pool)
                .await?;
        }

        // ── tag nodes + tagged edges ────────────────────────────────────────
        for tag in caption_tags {
            let tag_path = format!("image://{corpus_slug}/tag={tag}");
            let tag_id = upsert_image_node(
                pool,
                &tag_path,
                &tag,
                "image:tag",
                corpus_slug,
                &sha256_hex_bytes(format!("tag={tag}").as_bytes()),
            )
            .await?;
            stats.tags += 1;
            tag_facet(pool, corpus_id, tag_id, image_facet_id).await?;
            if add_image_edge(pool, file_id, tag_id, "tagged").await? {
                stats.edges += 1;
            }
        }
    }

    // Incremental GC: drop image:file nodes whose image vanished on disk (their
    // `tagged` edges cascade), then any image:tag now left with no incoming edge
    // (tag nodes are shared, so they're deleted only when nothing references
    // them — mirrors cortex's gc_orphan_placeholders). `path <> ALL('{}')` is
    // vacuously true, so an empty corpus correctly drops every image:file node.
    if incremental {
        sqlx::query(
            "DELETE FROM brain_vault_nodes
               WHERE project = $1 AND node_type = 'image:file'
                 AND path <> ALL($2)",
        )
        .bind(corpus_slug)
        .bind(&live_paths)
        .execute(pool)
        .await?;
        sqlx::query(
            "DELETE FROM brain_vault_nodes
               WHERE project = $1 AND node_type = 'image:tag'
                 AND NOT EXISTS (
                     SELECT 1 FROM brain_vault_edges e WHERE e.dst_id = brain_vault_nodes.id
                 )",
        )
        .bind(corpus_slug)
        .execute(pool)
        .await?;
    }

    Ok(stats)
}

// ─── vision pass ─────────────────────────────────────────────────────────────

/// POST one image to the fleet vision endpoint and parse a (caption, tags)
/// pair out of the model's reply. Returns Err on any HTTP/timeout/parse failure
/// so the caller can skip captioning for this image.
async fn caption_image(
    client: &reqwest::Client,
    bytes: &[u8],
    ext: &str,
) -> Result<(String, Vec<String>)> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let data_url = format!("data:{};base64,{}", mime_for_ext(ext), b64);
    let body = serde_json::json!({
        "model": VISION_MODEL,
        "max_tokens": 200,
        "temperature": 0.2,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image_url", "image_url": { "url": data_url } },
                { "type": "text",
                  "text": "Caption this image in one short sentence, then list 3-5 lowercase tags after 'TAGS:'." }
            ]
        }]
    });

    let resp = client.post(VISION_URL).json(&body).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("vision endpoint HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    let content = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("no content in vision response"))?;
    Ok(parse_caption_response(content))
}

/// Split a vision reply into (caption sentence, tags). Everything before a line
/// containing `TAGS:` (case-insensitive) is the caption; tags are the
/// comma/whitespace-separated lowercase tokens after it.
fn parse_caption_response(text: &str) -> (String, Vec<String>) {
    // Locate the TAGS: marker (case-insensitive) anywhere in the text.
    let lower = text.to_lowercase();
    let (caption, tags) = if let Some(idx) = lower.find("tags:") {
        let caption = text[..idx].trim().to_string();
        let tag_str = &text[idx + "tags:".len()..];
        (caption, extract_tags(tag_str))
    } else {
        (text.trim().to_string(), Vec::new())
    };
    // Collapse internal whitespace/newlines in the caption to a single line.
    let caption = caption.split_whitespace().collect::<Vec<_>>().join(" ");
    (caption, tags)
}

/// Parse the tag portion of a 'TAGS: a, b, c' line into a deduped, lowercase,
/// cleaned list. Splits on commas and whitespace; strips surrounding `#`, `.`,
/// quotes, and list bullets; drops empties.
fn extract_tags(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in s.split([',', '\n', ';']) {
        let cleaned: String = raw
            .trim()
            .trim_start_matches(['-', '*', '#', '•'])
            .trim()
            .trim_matches(['"', '\'', '.', '`'])
            .to_lowercase();
        let cleaned = cleaned.trim().to_string();
        if cleaned.is_empty() {
            continue;
        }
        // Skip absurdly long "tags" (a stray sentence) — keep it tag-shaped.
        if cleaned.len() > 40 {
            continue;
        }
        if !out.contains(&cleaned) {
            out.push(cleaned);
        }
    }
    out
}

// ─── DB helpers ──────────────────────────────────────────────────────────────

/// Upsert an image node by its synthetic unique `path`. Mirrors data_index's
/// upsert_data_node.
async fn upsert_image_node(
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
async fn add_image_edge(pool: &PgPool, src: Uuid, dst: Uuid, edge_type: &str) -> Result<bool> {
    let r = sqlx::query(
        r#"INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, provenance)
           VALUES ($1, $2, $3, 'cortex-image')
           ON CONFLICT (src_id, dst_id, edge_type) DO NOTHING"#,
    )
    .bind(src)
    .bind(dst)
    .bind(edge_type)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Ensure the corpus's modality=image facet exists; return its id. Matches the
/// SEED_FACETS row (`modality`, `image`, `Image`) and corpus.rs's upsert_facet.
async fn upsert_modality_image_facet(pool: &PgPool, corpus_id: Uuid) -> Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_facets (corpus_id, dimension, value, title)
           VALUES ($1, 'modality', 'image', 'Image')
           ON CONFLICT (corpus_id, dimension, value) DO UPDATE
             SET title = COALESCE(EXCLUDED.title, brain_facets.title)
           RETURNING id"#,
    )
    .bind(corpus_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Tag a node with the modality=image facet (idempotent).
async fn tag_facet(pool: &PgPool, corpus_id: Uuid, node_id: Uuid, facet_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO brain_node_facets (corpus_id, node_id, node_kind, facet_id, provenance)
           VALUES ($1, $2, 'content', $3, 'cortex-image')
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

/// Recursively collect image files under `root`, skipping vendored dirs and any
/// file larger than `MAX_IMAGE_BYTES`.
fn collect_image_files(root: &Path) -> Vec<PathBuf> {
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
                let is_img = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| is_image_ext(&e.to_lowercase()))
                    .unwrap_or(false);
                if !is_img {
                    continue;
                }
                // Skip oversized images.
                if let Ok(md) = entry.metadata() {
                    if md.len() > MAX_IMAGE_BYTES {
                        continue;
                    }
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn sha256_hex_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

/// On an incremental run, an image can skip the vision pass only when we've
/// indexed this exact path before with the identical content hash. A new image
/// (no prior node) or one whose bytes changed must be re-captioned.
fn image_unchanged(prior_hash: Option<&str>, current_hash: &str) -> bool {
    prior_hash == Some(current_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tags_from_tags_line() {
        let tags = extract_tags(" diagram, architecture, fleet , #cortex, diagram");
        // lowercased, '#' stripped, deduped (diagram appears twice).
        assert_eq!(tags, vec!["diagram", "architecture", "fleet", "cortex"]);
    }

    #[test]
    fn parses_caption_and_tags() {
        let reply =
            "A wiring diagram of the fleet topology.\nTAGS: diagram, network, fleet, topology";
        let (caption, tags) = parse_caption_response(reply);
        assert_eq!(caption, "A wiring diagram of the fleet topology.");
        assert_eq!(tags, vec!["diagram", "network", "fleet", "topology"]);
    }

    #[test]
    fn caption_without_tags_marker_keeps_full_text() {
        let (caption, tags) = parse_caption_response("Just a caption with no tags.");
        assert_eq!(caption, "Just a caption with no tags.");
        assert!(tags.is_empty());
    }

    #[test]
    fn image_ext_filter() {
        assert!(is_image_ext("png"));
        assert!(is_image_ext("jpeg"));
        assert!(is_image_ext("webp"));
        assert!(!is_image_ext("svg"));
        assert!(!is_image_ext("md"));
        assert!(!is_image_ext("csv"));
    }

    #[test]
    fn image_unchanged_only_when_hash_matches() {
        // Same path, identical bytes → skip the vision pass.
        assert!(image_unchanged(Some("abc"), "abc"));
        // Same path, bytes changed → must re-caption.
        assert!(!image_unchanged(Some("abc"), "def"));
        // New image (no prior node) → must caption.
        assert!(!image_unchanged(None, "abc"));
    }
}
