//! Faceted, multi-modal, multi-parent, multi-root knowledge graph under "Brain".
//!
//! INCREMENT 1: the STRUCTURE / FACETING layer. A faceted property graph
//! (a DAG of memberships), NOT a folder tree. Every content/entity node belongs
//! to MANY entities + carries MANY facets at once; queries are SET INTERSECTIONS.
//!
//! Reuses the cached `PgPool` (passed in, like vault.rs / vector_search.rs) —
//! never builds a PgPool per call. The deterministic auto-proposer was attempted
//! on the fleet (fleet_run tier-2) but the dispatch timed out twice; the builder
//! wrote it inline.

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    ".next",
    "__pycache__",
    ".venv",
    "venv",
    ".cache",
    "build",
];

const SEED_FACETS: &[(&str, &str, &str)] = &[
    ("role", "frontend", "Frontend"),
    ("role", "backend", "Backend"),
    ("role", "ml", "ML"),
    ("role", "docs", "Docs"),
    ("role", "business", "Business"),
    ("role", "finance", "Finance"),
    ("role", "product", "Product"),
    ("status", "active", "Active"),
    ("status", "legacy", "Legacy"),
    ("modality", "code", "Code"),
    ("modality", "doc", "Doc"),
    ("modality", "image", "Image"),
    ("modality", "data", "Data"),
];

#[derive(Debug, Clone)]
pub struct Corpus {
    pub id: Uuid,
    pub slug: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub id: Uuid,
    pub root_path: String,
    pub label: Option<String>,
    pub scan_status: String,
    pub file_count: i32,
}

#[derive(Debug, Clone)]
pub struct CorpusSummary {
    pub slug: String,
    pub title: String,
    pub sources: i64,
    pub entities: i64,
    pub facets: i64,
    pub content: i64,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: String,
    pub title: String,
    pub payload: serde_json::Value,
    pub heuristic: String,
    pub confidence: f32,
}

#[derive(Debug, Clone)]
pub struct StoredCandidate {
    pub id: Uuid,
    pub kind: String,
    pub title: String,
    pub payload: serde_json::Value,
    pub heuristic: Option<String>,
    pub confidence: f32,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ScanReport {
    pub sources_scanned: usize,
    pub dirs: usize,
    pub files: usize,
    pub nodes_upserted: usize,
    pub edges: usize,
    pub candidates: usize,
    /// Current content:% nodes of this corpus whose path lies outside every
    /// registered source root, invalidated (valid_until) at the end of scan().
    pub pruned: usize,
}

#[derive(Debug, Clone)]
pub struct QueryRow {
    pub id: Uuid,
    pub path: String,
    pub title: String,
    pub node_type: String,
}

pub async fn add_corpus(
    pg: &PgPool,
    slug: &str,
    title: &str,
    roots: &[(String, Option<String>)],
) -> anyhow::Result<Corpus> {
    let row = sqlx::query(
        r#"INSERT INTO brain_corpora (slug, title)
           VALUES ($1, $2)
           ON CONFLICT (slug) DO UPDATE SET title = EXCLUDED.title, updated_at = NOW()
           RETURNING id, slug, title"#,
    )
    .bind(slug)
    .bind(title)
    .fetch_one(pg)
    .await?;
    let corpus = Corpus {
        id: row.get("id"),
        slug: row.get("slug"),
        title: row.get("title"),
    };

    for (root, label) in roots {
        add_source(pg, &corpus, root, label.as_deref()).await?;
    }
    for (dim, val, disp) in SEED_FACETS {
        sqlx::query(
            r#"INSERT INTO brain_facets (corpus_id, dimension, value, title)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (corpus_id, dimension, value) DO NOTHING"#,
        )
        .bind(corpus.id)
        .bind(dim)
        .bind(val)
        .bind(disp)
        .execute(pg)
        .await?;
    }
    Ok(corpus)
}

pub async fn add_source(
    pg: &PgPool,
    corpus: &Corpus,
    root_path: &str,
    label: Option<&str>,
) -> anyhow::Result<Source> {
    let abs = expand_path(root_path);
    // root_path is GLOBALLY unique (uq_sources_root): a directory belongs to
    // exactly one corpus at a time. Registering it under a new corpus MUST
    // reassign corpus_id — otherwise the source silently stays owned by the
    // previous corpus, list_sources() for the new corpus returns nothing, and
    // scan() walks zero files (the `ff cortex index --slug <new>` files=0 bug
    // when the same dir was earlier indexed under a different slug).
    let row = sqlx::query(
        r#"INSERT INTO brain_sources (corpus_id, root_path, label)
           VALUES ($1, $2, $3)
           ON CONFLICT (root_path) DO UPDATE
             SET corpus_id = EXCLUDED.corpus_id,
                 label = COALESCE(EXCLUDED.label, brain_sources.label)
           RETURNING id, root_path, label, scan_status, file_count"#,
    )
    .bind(corpus.id)
    .bind(&abs)
    .bind(label)
    .fetch_one(pg)
    .await?;
    Ok(Source {
        id: row.get("id"),
        root_path: row.get("root_path"),
        label: row.get("label"),
        scan_status: row.get("scan_status"),
        file_count: row.get("file_count"),
    })
}

pub async fn get_corpus(pg: &PgPool, slug: &str) -> anyhow::Result<Option<Corpus>> {
    let row = sqlx::query("SELECT id, slug, title FROM brain_corpora WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pg)
        .await?;
    Ok(row.map(|r| Corpus {
        id: r.get("id"),
        slug: r.get("slug"),
        title: r.get("title"),
    }))
}

/// Delete a corpus and everything scoped to it. Returns
/// `(nodes_deleted, corpora_deleted)` — `corpora_deleted` is 0 when the slug
/// didn't exist. Nodes are matched by `project = slug` (how scan/cortex tag
/// them); their edges cascade via brain_vault_edges FKs. Sources, entities,
/// facets, memberships, and candidates cascade off the brain_corpora row.
pub async fn delete_corpus(pg: &PgPool, slug: &str) -> anyhow::Result<(u64, u64)> {
    let mut tx = pg.begin().await?;
    let nodes = sqlx::query("DELETE FROM brain_vault_nodes WHERE project = $1")
        .bind(slug)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let corpora = sqlx::query("DELETE FROM brain_corpora WHERE slug = $1")
        .bind(slug)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok((nodes, corpora))
}

pub async fn list_sources(pg: &PgPool, corpus: &Corpus) -> anyhow::Result<Vec<Source>> {
    let rows = sqlx::query(
        r#"SELECT id, root_path, label, scan_status, file_count
           FROM brain_sources WHERE corpus_id = $1
           ORDER BY root_path COLLATE "C""#,
    )
    .bind(corpus.id)
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Source {
            id: r.get("id"),
            root_path: r.get("root_path"),
            label: r.get("label"),
            scan_status: r.get("scan_status"),
            file_count: r.get("file_count"),
        })
        .collect())
}

pub async fn list_corpora(pg: &PgPool) -> anyhow::Result<Vec<CorpusSummary>> {
    let rows = sqlx::query(
        r#"SELECT c.slug, c.title,
                  (SELECT COUNT(*) FROM brain_sources s WHERE s.corpus_id = c.id)  AS sources,
                  (SELECT COUNT(*) FROM brain_entities e WHERE e.corpus_id = c.id)  AS entities,
                  (SELECT COUNT(*) FROM brain_facets f WHERE f.corpus_id = c.id)    AS facets,
                  (SELECT COUNT(*) FROM brain_vault_nodes n
                     WHERE n.project = c.slug AND n.valid_until IS NULL
                       AND n.node_type LIKE 'content:%')                            AS content
           FROM brain_corpora c
           ORDER BY c.slug COLLATE "C""#,
    )
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CorpusSummary {
            slug: r.get("slug"),
            title: r.get("title"),
            sources: r.get("sources"),
            entities: r.get("entities"),
            facets: r.get("facets"),
            content: r.get("content"),
        })
        .collect())
}

pub async fn scan(
    pg: &PgPool,
    corpus: &Corpus,
    only_source: Option<&str>,
    max_depth: usize,
) -> anyhow::Result<ScanReport> {
    let sources = list_sources(pg, corpus).await?;
    let mut report = ScanReport {
        sources_scanned: 0,
        dirs: 0,
        files: 0,
        nodes_upserted: 0,
        edges: 0,
        candidates: 0,
        pruned: 0,
    };

    let mut all_paths: Vec<PathBuf> = Vec::new();
    let mut all_dirs: Vec<PathBuf> = Vec::new();
    let mut source_roots: Vec<(PathBuf, Option<String>)> = Vec::new();

    for source in &sources {
        if let Some(only) = only_source {
            if expand_path(only) != source.root_path {
                continue;
            }
        }
        let root = PathBuf::from(&source.root_path);
        if !root.exists() {
            continue;
        }
        source_roots.push((root.clone(), source.label.clone()));

        let walked = walk(&root, max_depth);
        let mut path_id: HashMap<PathBuf, Uuid> = HashMap::new();

        // Pre-load this corpus's CURRENT (valid) content nodes + this source's
        // node→rel_path mappings so an UNCHANGED file/dir can skip its per-row
        // writes. `cheap_hash` already folds size+mtime, so a matching
        // content_hash (with identical title/node_type, still valid, owned by
        // this corpus) means the upsert below would be a pure no-op. A no-op
        // rescan thus collapses from ~3 sequential DB round-trips per entry
        // (content upsert + node_sources upsert + contains-edge insert) to a
        // single batch read — the dominant cost of the hook-driven
        // `ff cortex index --incremental`, which rescans the whole corpus on
        // every commit. End-state is identical: we only skip writes the upsert
        // would have made no-ops (content_hash is the change signal cortex/doc/
        // image indexing and the out-of-root prune all key on).
        let existing: HashMap<String, (Uuid, String, String, String)> = sqlx::query(
            r#"SELECT path, id, content_hash, title, node_type
                 FROM brain_vault_nodes
                WHERE project = $1 AND node_type LIKE 'content:%'
                  AND valid_until IS NULL"#,
        )
        .bind(&corpus.slug)
        .fetch_all(pg)
        .await?
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("path"),
                (
                    r.get::<Uuid, _>("id"),
                    r.get::<String, _>("content_hash"),
                    r.get::<String, _>("title"),
                    r.get::<String, _>("node_type"),
                ),
            )
        })
        .collect();
        let existing_src: HashMap<Uuid, String> =
            sqlx::query("SELECT node_id, rel_path FROM brain_node_sources WHERE source_id = $1")
                .bind(source.id)
                .fetch_all(pg)
                .await?
                .into_iter()
                .map(|r| (r.get::<Uuid, _>("node_id"), r.get::<String, _>("rel_path")))
                .collect();
        // Entries whose node AND source-mapping were both already present and
        // unchanged this scan — their `contains` edge was created by the prior
        // scan that inserted them, so the edge insert can be skipped too.
        let mut reused: HashSet<PathBuf> = HashSet::new();

        for entry in &walked {
            let abs = entry.path.to_string_lossy().to_string();
            let title = entry
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| abs.clone());
            let node_type = if entry.is_dir {
                "content:dir"
            } else {
                "content:file"
            };
            let hash = cheap_hash(&abs, entry.size, entry.mtime);

            let rel = entry
                .path
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            // Reuse the stored row when it is byte-identical (same hash, title,
            // node_type — and already valid + owned by this corpus, which the
            // `existing` query guarantees); otherwise upsert.
            let (id, node_unchanged) = match existing.get(&abs) {
                Some((eid, h, t, nt)) if *h == hash && *t == title && nt == node_type => {
                    (*eid, true)
                }
                _ => (
                    upsert_content_node(pg, &abs, &title, node_type, &corpus.slug, &hash).await?,
                    false,
                ),
            };
            path_id.insert(entry.path.clone(), id);
            report.nodes_upserted += 1;
            if entry.is_dir {
                report.dirs += 1;
            } else {
                report.files += 1;
            }

            // node→source mapping: skip when it already maps this exact rel_path.
            let src_unchanged = existing_src.get(&id).is_some_and(|r| *r == rel);
            if !src_unchanged {
                sqlx::query(
                    r#"INSERT INTO brain_node_sources (node_id, source_id, rel_path)
                       VALUES ($1, $2, $3)
                       ON CONFLICT (node_id, source_id) DO UPDATE SET rel_path = EXCLUDED.rel_path"#,
                )
                .bind(id)
                .bind(source.id)
                .bind(&rel)
                .execute(pg)
                .await?;
            }

            if node_unchanged && src_unchanged {
                reused.insert(entry.path.clone());
            }

            if entry.is_dir {
                all_dirs.push(entry.path.clone());
            }
            all_paths.push(entry.path.clone());
        }

        for entry in &walked {
            if let Some(parent) = entry.path.parent() {
                // Both endpoints reused unchanged ⇒ the `contains` edge already
                // exists from the prior scan that inserted them — skip the write.
                if reused.contains(&entry.path) && reused.contains(parent) {
                    continue;
                }
                if let (Some(&src), Some(&dst)) = (path_id.get(parent), path_id.get(&entry.path)) {
                    let r = sqlx::query(
                        r#"INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, provenance)
                           VALUES ($1, $2, 'contains', 'scan')
                           ON CONFLICT (src_id, dst_id, edge_type) DO NOTHING"#,
                    )
                    .bind(src)
                    .bind(dst)
                    .execute(pg)
                    .await?;
                    report.edges += r.rows_affected() as usize;
                }
            }
        }

        sqlx::query(
            r#"UPDATE brain_sources
               SET scan_status = 'scanned', last_scanned = NOW(), file_count = $2
               WHERE id = $1"#,
        )
        .bind(source.id)
        .bind(walked.len() as i32)
        .execute(pg)
        .await?;
        report.sources_scanned += 1;
    }

    // Corpus-scoping: content nodes are single-owner (brain_vault_nodes.path is
    // globally UNIQUE and the upsert reassigns `project`), so this corpus can
    // hold stale content:% rows for paths it no longer covers — e.g. a backup
    // dir once mis-scanned into this slug, or files that vanished from disk.
    // Invalidate every CURRENT content:% row of this corpus whose path lies
    // outside ALL of its registered source roots, so cortex/doc/facet queries
    // never see foreign files. Skipped when the corpus has no sources at all
    // (nothing to scope against — better to keep than to wipe).
    if !sources.is_empty() {
        let pruned = sqlx::query(
            r#"UPDATE brain_vault_nodes n
                  SET valid_until = NOW(), updated_at = NOW()
                WHERE n.project = $1
                  AND n.node_type LIKE 'content:%'
                  AND n.valid_until IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM brain_sources s
                       WHERE s.corpus_id = $2
                         AND (n.path = s.root_path
                              OR n.path LIKE s.root_path || '/%')
                  )"#,
        )
        .bind(&corpus.slug)
        .bind(corpus.id)
        .execute(pg)
        .await?;
        report.pruned = pruned.rows_affected() as usize;
    }

    let dir_set: HashSet<PathBuf> = all_dirs.into_iter().collect();
    let candidates = propose(&all_paths, &dir_set, &source_roots);
    sqlx::query("DELETE FROM brain_corpus_candidates WHERE corpus_id = $1 AND status = 'pending'")
        .bind(corpus.id)
        .execute(pg)
        .await?;
    for c in &candidates {
        sqlx::query(
            r#"INSERT INTO brain_corpus_candidates
                 (corpus_id, kind, title, payload, heuristic, confidence)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(corpus.id)
        .bind(&c.kind)
        .bind(&c.title)
        .bind(&c.payload)
        .bind(&c.heuristic)
        .bind(c.confidence)
        .execute(pg)
        .await?;
    }
    report.candidates = candidates.len();
    Ok(report)
}

/// Upsert a content node keyed by absolute path. `path` is globally UNIQUE in
/// brain_vault_nodes, so content is single-owner across corpora: re-scanning a
/// directory under a different corpus slug intentionally REASSIGNS `project`
/// to the scanning corpus (last scan wins). The previous corpus drops the rows
/// from its content queries immediately; scan()'s out-of-root prune clears any
/// residue the next time that corpus is scanned.
async fn upsert_content_node(
    pg: &PgPool,
    path: &str,
    title: &str,
    node_type: &str,
    project: &str,
    content_hash: &str,
) -> anyhow::Result<Uuid> {
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
    .fetch_one(pg)
    .await?;
    Ok(id)
}

pub async fn list_candidates(
    pg: &PgPool,
    corpus: &Corpus,
    status: Option<&str>,
    kind: Option<&str>,
) -> anyhow::Result<Vec<StoredCandidate>> {
    let rows = sqlx::query(
        r#"SELECT id, kind, title, payload, heuristic, confidence, status
           FROM brain_corpus_candidates
           WHERE corpus_id = $1
             AND ($2::text IS NULL OR status = $2)
             AND ($3::text IS NULL OR kind = $3)
           ORDER BY kind COLLATE "C", confidence DESC, title COLLATE "C""#,
    )
    .bind(corpus.id)
    .bind(status)
    .bind(kind)
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| StoredCandidate {
            id: r.get("id"),
            kind: r.get("kind"),
            title: r.get::<Option<String>, _>("title").unwrap_or_default(),
            payload: r.get("payload"),
            heuristic: r.get("heuristic"),
            confidence: r.get("confidence"),
            status: r.get("status"),
        })
        .collect())
}

pub async fn confirm_candidates(
    pg: &PgPool,
    corpus: &Corpus,
    ids: &[Uuid],
    all: bool,
    min_conf: f32,
) -> anyhow::Result<usize> {
    let pending = list_candidates(pg, corpus, Some("pending"), None).await?;
    let want: HashSet<Uuid> = ids.iter().copied().collect();
    let mut confirmed = 0usize;

    let order = |k: &str| match k {
        "group_entity" => 0,
        "product_entity" => 1,
        "unit_entity" => 2,
        "facet" => 3,
        "membership" => 4,
        "facet_assign" => 5,
        _ => 6,
    };
    let mut chosen: Vec<&StoredCandidate> = pending
        .iter()
        .filter(|c| all || want.contains(&c.id))
        .filter(|c| (all && c.confidence >= min_conf) || !all)
        .collect();
    chosen.sort_by_key(|c| order(&c.kind));

    for c in chosen {
        materialize_one(pg, corpus, c).await?;
        sqlx::query(
            "UPDATE brain_corpus_candidates SET status='confirmed', reviewed_at=NOW() WHERE id=$1",
        )
        .bind(c.id)
        .execute(pg)
        .await?;
        confirmed += 1;
    }
    Ok(confirmed)
}

pub async fn reject_candidates(pg: &PgPool, ids: &[Uuid]) -> anyhow::Result<usize> {
    let mut n = 0;
    for id in ids {
        let r = sqlx::query(
            "UPDATE brain_corpus_candidates SET status='rejected', reviewed_at=NOW() WHERE id=$1",
        )
        .bind(id)
        .execute(pg)
        .await?;
        n += r.rows_affected() as usize;
    }
    Ok(n)
}

async fn materialize_one(pg: &PgPool, corpus: &Corpus, c: &StoredCandidate) -> anyhow::Result<()> {
    let p = &c.payload;
    match c.kind.as_str() {
        "group_entity" | "unit_entity" | "product_entity" | "project_entity" => {
            let key = p.get("entity_key").and_then(|v| v.as_str()).unwrap_or("");
            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or(key);
            let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("unit");
            let primary = p.get("primary_path").and_then(|v| v.as_str());
            let parent_key = p.get("parent_entity_key").and_then(|v| v.as_str());
            let eid =
                upsert_entity(pg, corpus, key, name, kind, parent_key, primary, "auto").await?;
            // Tie every content node at-or-under the entity's primary_path to the
            // entity as member_kind='content', so faceted --entity/--product
            // queries (which intersect content<->entity) surface this content.
            if let Some(dir) = primary {
                link_content_under_path(pg, corpus, eid, dir).await?;
            }
        }
        "facet" => {
            let dim = p.get("dimension").and_then(|v| v.as_str()).unwrap_or("");
            let val = p.get("value").and_then(|v| v.as_str()).unwrap_or("");
            let title = p.get("title").and_then(|v| v.as_str());
            upsert_facet(pg, corpus, dim, val, title).await?;
        }
        "facet_assign" => {
            let dim = p.get("dimension").and_then(|v| v.as_str()).unwrap_or("");
            let val = p.get("value").and_then(|v| v.as_str()).unwrap_or("");
            upsert_facet(pg, corpus, dim, val, None).await?;
            if let Some(path) = p.get("node_path").and_then(|v| v.as_str()) {
                if let Some(node_id) = content_node_id(pg, path).await? {
                    assign_facet(pg, corpus, node_id, "content", dim, val, "auto").await?;
                }
                // If the path is a directory, propagate the facet to ALL content
                // under it so faceted intersection (content<->facet) covers the
                // whole subtree (e.g. role:backend over the entire profilex-api).
                propagate_facet_under_path(pg, corpus, path, dim, val).await?;
            }
        }
        "membership" => {
            let entity_key = p.get("entity_key").and_then(|v| v.as_str()).unwrap_or("");
            let relation = p
                .get("relation")
                .and_then(|v| v.as_str())
                .unwrap_or("member_of");
            let member_kind = p
                .get("member_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("content");
            let Some(entity_id) = entity_id_by_key(pg, corpus, entity_key).await? else {
                return Ok(());
            };
            let member_id = if member_kind == "entity" {
                let mk = p
                    .get("member_entity_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                entity_id_by_key(pg, corpus, mk).await?
            } else {
                let path = p.get("member_path").and_then(|v| v.as_str()).unwrap_or("");
                content_node_id(pg, path).await?
            };
            if let Some(mid) = member_id {
                add_membership(pg, corpus, mid, member_kind, entity_id, relation, "auto").await?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_entity(
    pg: &PgPool,
    corpus: &Corpus,
    key: &str,
    name: &str,
    kind: &str,
    parent_key: Option<&str>,
    primary_path: Option<&str>,
    provenance: &str,
) -> anyhow::Result<Uuid> {
    let parent_id = match parent_key {
        Some(pk) => entity_id_by_key(pg, corpus, pk).await?,
        None => None,
    };
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_entities
             (corpus_id, entity_key, name, entity_kind, parent_entity_id, primary_path, provenance)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           ON CONFLICT (corpus_id, entity_key) DO UPDATE
             SET name = EXCLUDED.name, entity_kind = EXCLUDED.entity_kind,
                 parent_entity_id = COALESCE(EXCLUDED.parent_entity_id, brain_entities.parent_entity_id),
                 primary_path = COALESCE(EXCLUDED.primary_path, brain_entities.primary_path),
                 updated_at = NOW()
           RETURNING id"#,
    )
    .bind(corpus.id).bind(key).bind(name).bind(kind).bind(parent_id)
    .bind(primary_path).bind(provenance).fetch_one(pg).await?;
    Ok(id)
}

pub async fn upsert_facet(
    pg: &PgPool,
    corpus: &Corpus,
    dimension: &str,
    value: &str,
    title: Option<&str>,
) -> anyhow::Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_facets (corpus_id, dimension, value, title)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (corpus_id, dimension, value) DO UPDATE
             SET title = COALESCE(EXCLUDED.title, brain_facets.title)
           RETURNING id"#,
    )
    .bind(corpus.id)
    .bind(dimension)
    .bind(value)
    .bind(title)
    .fetch_one(pg)
    .await?;
    Ok(id)
}

pub async fn add_membership(
    pg: &PgPool,
    corpus: &Corpus,
    member_id: Uuid,
    member_kind: &str,
    entity_id: Uuid,
    relation: &str,
    provenance: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO brain_memberships
             (corpus_id, member_id, member_kind, entity_id, relation, provenance)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (member_id, entity_id, relation) DO NOTHING"#,
    )
    .bind(corpus.id)
    .bind(member_id)
    .bind(member_kind)
    .bind(entity_id)
    .bind(relation)
    .bind(provenance)
    .execute(pg)
    .await?;
    Ok(())
}

/// Link every CURRENT content node whose path == dir OR is under dir/ to the
/// entity as a content membership. Idempotent.
pub async fn link_content_under_path(
    pg: &PgPool,
    corpus: &Corpus,
    entity_id: Uuid,
    dir: &str,
) -> anyhow::Result<u64> {
    let like = format!("{dir}/%");
    let r = sqlx::query(
        r#"INSERT INTO brain_memberships
             (corpus_id, member_id, member_kind, entity_id, relation, provenance)
           SELECT $1, n.id, 'content', $2, 'member_of', 'auto'
             FROM brain_vault_nodes n
            WHERE n.valid_until IS NULL
              AND n.node_type LIKE 'content:%'
              AND (n.path = $3 OR n.path LIKE $4)
           ON CONFLICT (member_id, entity_id, relation) DO NOTHING"#,
    )
    .bind(corpus.id)
    .bind(entity_id)
    .bind(dir)
    .bind(&like)
    .execute(pg)
    .await?;
    Ok(r.rows_affected())
}

/// Assign (dimension,value) to EVERY content node under `dir/` (subtree). Used
/// so a dir-level role/status facet covers all descendant files in intersections.
pub async fn propagate_facet_under_path(
    pg: &PgPool,
    corpus: &Corpus,
    dir: &str,
    dimension: &str,
    value: &str,
) -> anyhow::Result<u64> {
    let facet_id = upsert_facet(pg, corpus, dimension, value, None).await?;
    let like = format!("{dir}/%");
    let r = sqlx::query(
        r#"INSERT INTO brain_node_facets (corpus_id, node_id, node_kind, facet_id, provenance)
           SELECT $1, n.id, 'content', $2, 'auto'
             FROM brain_vault_nodes n
            WHERE n.valid_until IS NULL
              AND n.node_type LIKE 'content:%'
              AND n.path LIKE $3
           ON CONFLICT (node_id, facet_id) DO NOTHING"#,
    )
    .bind(corpus.id)
    .bind(facet_id)
    .bind(&like)
    .execute(pg)
    .await?;
    Ok(r.rows_affected())
}

pub async fn assign_facet(
    pg: &PgPool,
    corpus: &Corpus,
    node_id: Uuid,
    node_kind: &str,
    dimension: &str,
    value: &str,
    provenance: &str,
) -> anyhow::Result<()> {
    let facet_id = upsert_facet(pg, corpus, dimension, value, None).await?;
    sqlx::query(
        r#"INSERT INTO brain_node_facets (corpus_id, node_id, node_kind, facet_id, provenance)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (node_id, facet_id) DO NOTHING"#,
    )
    .bind(corpus.id)
    .bind(node_id)
    .bind(node_kind)
    .bind(facet_id)
    .bind(provenance)
    .execute(pg)
    .await?;
    Ok(())
}

async fn entity_id_by_key(pg: &PgPool, corpus: &Corpus, key: &str) -> anyhow::Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar(
            "SELECT id FROM brain_entities WHERE corpus_id = $1 AND entity_key = $2",
        )
        .bind(corpus.id)
        .bind(key)
        .fetch_optional(pg)
        .await?,
    )
}

async fn content_node_id(pg: &PgPool, path: &str) -> anyhow::Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM brain_vault_nodes WHERE path = $1 AND valid_until IS NULL",
    )
    .bind(path)
    .fetch_optional(pg)
    .await?)
}

#[derive(Debug, Default, Clone)]
pub struct FacetQuery {
    pub entities: Vec<String>,
    pub facets: Vec<(String, String)>,
}

pub async fn query(pg: &PgPool, corpus: &Corpus, q: &FacetQuery) -> anyhow::Result<Vec<QueryRow>> {
    let mut sql = String::from(
        r#"WITH scoped AS (
              SELECT DISTINCT ns.node_id
              FROM brain_node_sources ns
              JOIN brain_sources s ON s.id = ns.source_id
              WHERE s.corpus_id = $1
           )
           SELECT n.id, n.path, n.title, n.node_type
           FROM brain_vault_nodes n
           JOIN scoped sc ON sc.node_id = n.id
           WHERE n.valid_until IS NULL
             AND n.node_type LIKE 'content:%'"#,
    );

    let mut entity_id_sets: Vec<Vec<Uuid>> = Vec::new();
    for key in &q.entities {
        let ids = entity_closure(pg, corpus, key).await?;
        if ids.is_empty() {
            return Ok(vec![]);
        }
        entity_id_sets.push(ids);
    }

    let mut by_dim: HashMap<String, Vec<String>> = HashMap::new();
    for (dim, val) in &q.facets {
        by_dim.entry(dim.clone()).or_default().push(val.clone());
    }

    let mut binds_uuid_lists: Vec<Vec<Uuid>> = Vec::new();
    let mut binds_dim: Vec<String> = Vec::new();
    let mut binds_vals: Vec<Vec<String>> = Vec::new();

    let mut next_idx = 2;
    for ids in &entity_id_sets {
        sql.push_str(&format!(
            r#" AND EXISTS (SELECT 1 FROM brain_memberships m
                 WHERE m.member_id = n.id AND m.member_kind = 'content'
                   AND m.entity_id = ANY(${idx}))"#,
            idx = next_idx
        ));
        binds_uuid_lists.push(ids.clone());
        next_idx += 1;
    }

    let mut dims: Vec<String> = by_dim.keys().cloned().collect();
    dims.sort();
    for dim in dims {
        let vals = by_dim.remove(&dim).unwrap();
        let dim_idx = next_idx;
        let val_idx = next_idx + 1;
        sql.push_str(&format!(
            r#" AND EXISTS (SELECT 1 FROM brain_node_facets nf
                 JOIN brain_facets f ON f.id = nf.facet_id
                 WHERE nf.node_id = n.id AND f.dimension = ${d} AND f.value = ANY(${v}))"#,
            d = dim_idx,
            v = val_idx
        ));
        binds_dim.push(dim);
        binds_vals.push(vals);
        next_idx += 2;
    }

    sql.push_str(r#" ORDER BY n.path COLLATE "C""#);

    let mut qb = sqlx::query(&sql).bind(corpus.id);
    for ids in &binds_uuid_lists {
        qb = qb.bind(ids);
    }
    for (dim, vals) in binds_dim.iter().zip(binds_vals.iter()) {
        qb = qb.bind(dim).bind(vals);
    }

    let rows = qb.fetch_all(pg).await?;
    Ok(rows
        .into_iter()
        .map(|r| QueryRow {
            id: r.get("id"),
            path: r.get("path"),
            title: r.get("title"),
            node_type: r.get::<Option<String>, _>("node_type").unwrap_or_default(),
        })
        .collect())
}

async fn entity_closure(pg: &PgPool, corpus: &Corpus, key: &str) -> anyhow::Result<Vec<Uuid>> {
    // The closure of an entity = the entity itself + all descendants reachable
    // EITHER by parent_entity_id (the entity tree) OR by an entity-type
    // membership (e.g. product 'profilex' has unit members app-profilex,
    // website-profilex, profilex-api). This is what makes a cross-folder product
    // a single set. Cycles are bounded by UNION (visited-set) semantics.
    let rows = sqlx::query(
        r#"WITH RECURSIVE walk AS (
               SELECT e.id FROM brain_entities e
                WHERE e.corpus_id = $1 AND e.entity_key = $2
             UNION
               SELECT child.id FROM walk w
               JOIN LATERAL (
                   SELECT ch.id FROM brain_entities ch
                     WHERE ch.parent_entity_id = w.id
                   UNION
                   SELECT m.member_id AS id FROM brain_memberships m
                     WHERE m.entity_id = w.id AND m.member_kind = 'entity'
               ) AS child ON TRUE
           )
           SELECT id FROM walk"#,
    )
    .bind(corpus.id)
    .bind(key)
    .fetch_all(pg)
    .await?;
    Ok(rows.into_iter().map(|r| r.get::<Uuid, _>("id")).collect())
}

pub fn propose(
    paths: &[PathBuf],
    dirs: &HashSet<PathBuf>,
    sources: &[(PathBuf, Option<String>)],
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();

    // A path is a DIR if it's in the explicit dirs set (from the real scan) OR,
    // as a fallback when dirs is empty (e.g. unit tests), if it parents another path.
    let parents: HashSet<PathBuf> = paths
        .iter()
        .filter_map(|p| p.parent().map(|x| x.to_path_buf()))
        .collect();
    let is_dir = |p: &PathBuf| dirs.contains(p) || (dirs.is_empty() && parents.contains(p));
    let name_of = |p: &PathBuf| {
        p.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    let business_roots: HashSet<&PathBuf> = sources
        .iter()
        .filter(|(_, l)| l.as_deref() == Some("business"))
        .map(|(r, _)| r)
        .collect();

    let mut group_dirs: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !is_dir(p) {
            continue;
        }
        let n = name_of(p);
        if n == "apps" || n == "services" {
            group_dirs.push(p.clone());
            let heur = if n == "apps" {
                "apps_dir"
            } else {
                "services_dir"
            };
            out.push(Candidate {
                kind: "group_entity".into(),
                title: n.clone(),
                payload: json!({"entity_key": n, "name": n, "kind": "group", "primary_path": p.to_string_lossy()}),
                heuristic: heur.into(),
                confidence: 0.9,
            });
        }
    }

    let group_set: HashSet<&PathBuf> = group_dirs.iter().collect();
    let mut units: Vec<(String, PathBuf)> = Vec::new();
    for p in paths {
        if !is_dir(p) {
            continue;
        }
        if let Some(parent) = p.parent() {
            if group_set.contains(&parent.to_path_buf()) {
                let n = name_of(p);
                let gname = name_of(&parent.to_path_buf());
                units.push((n.clone(), p.clone()));
                out.push(Candidate {
                    kind: "unit_entity".into(),
                    title: n.clone(),
                    payload: json!({"entity_key": n, "name": n, "kind": "unit", "parent_entity_key": gname, "primary_path": p.to_string_lossy()}),
                    heuristic: "group_child".into(),
                    confidence: 0.85,
                });
                // Role by layer: services/* => backend, apps/* => frontend.
                let role = match gname.as_str() {
                    "services" => Some("backend"),
                    "apps" => Some("frontend"),
                    _ => None,
                };
                if let Some(role) = role {
                    out.push(Candidate {
                        kind: "facet_assign".into(),
                        title: format!("{n} => role:{role}"),
                        payload: json!({"node_path": p.to_string_lossy(), "dimension": "role", "value": role}),
                        heuristic: "layer_role".into(),
                        confidence: 0.75,
                    });
                }
            }
        }
    }

    for stem in ["profilex", "hireflow360"] {
        let matched: Vec<&String> = units
            .iter()
            .map(|(n, _)| n)
            .filter(|n| n.to_lowercase().contains(stem))
            .collect();
        if matched.len() >= 2 {
            let cap = if stem == "profilex" {
                "ProfileX"
            } else {
                "HireFlow360"
            };
            out.push(Candidate {
                kind: "product_entity".into(),
                title: stem.into(),
                payload: json!({"entity_key": stem, "name": cap, "kind": "product", "stem": stem, "facet": format!("product:{stem}")}),
                heuristic: "name_stem".into(),
                confidence: 0.8,
            });
            out.push(Candidate {
                kind: "facet".into(),
                title: format!("product:{stem}"),
                payload: json!({"dimension": "product", "value": stem, "title": cap}),
                heuristic: "name_stem".into(),
                confidence: 0.8,
            });
            for u in matched {
                out.push(Candidate {
                    kind: "membership".into(),
                    title: format!("{stem} <- {u}"),
                    payload: json!({"member_entity_key": u, "member_kind": "entity", "entity_key": stem, "relation": "member_of"}),
                    heuristic: "name_stem".into(),
                    confidence: 0.8,
                });
            }
        }
    }

    for p in paths {
        if !is_dir(p) {
            continue;
        }
        let n = name_of(p);
        let abs = p.to_string_lossy().to_string();
        let is_business = business_roots.contains(&p) || n == "business";
        if is_business {
            out.push(Candidate {
                kind: "facet_assign".into(),
                title: format!("{n} => role:business"),
                payload: json!({"node_path": abs, "dimension": "role", "value": "business"}),
                heuristic: "business_root".into(),
                confidence: 0.7,
            });
        } else if n == "Receipts" || n == "Revenue streams" || n == "Revenue Model" {
            out.push(Candidate {
                kind: "facet_assign".into(),
                title: format!("{n} => role:finance"),
                payload: json!({"node_path": abs, "dimension": "role", "value": "finance"}),
                heuristic: "finance_dir".into(),
                confidence: 0.7,
            });
        } else if n.starts_with("docs") || n == "developer-docs" {
            out.push(Candidate {
                kind: "facet_assign".into(),
                title: format!("{n} => role:docs"),
                payload: json!({"node_path": abs, "dimension": "role", "value": "docs"}),
                heuristic: "docs_dir".into(),
                confidence: 0.7,
            });
        } else if n == "old-repos" {
            out.push(Candidate {
                kind: "facet_assign".into(),
                title: format!("{n} => status:legacy"),
                payload: json!({"node_path": abs, "dimension": "status", "value": "legacy"}),
                heuristic: "legacy_dir".into(),
                confidence: 0.7,
            });
        }
    }

    for p in paths {
        if is_dir(p) {
            continue;
        }
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let modality = match ext.as_str() {
            "rs" | "ts" | "py" | "go" | "js" => "code",
            "md" | "pdf" | "docx" => "doc",
            "png" | "jpg" | "jpeg" | "svg" => "image",
            "csv" | "json" | "parquet" => "data",
            _ => continue,
        };
        let n = name_of(p);
        out.push(Candidate {
            kind: "facet_assign".into(),
            title: format!("{n} => modality:{modality}"),
            payload: json!({"node_path": p.to_string_lossy(), "dimension": "modality", "value": modality}),
            heuristic: "file_ext".into(),
            confidence: 0.6,
        });
    }

    out.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.title.cmp(&b.title)));
    out
}

struct Walked {
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

fn walk(root: &Path, max_depth: usize) -> Vec<Walked> {
    let mut out = Vec::new();
    walk_inner(root, root, 0, max_depth, &mut out);
    out
}

fn walk_inner(root: &Path, dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<Walked>) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    if depth == 0 {
        if let Ok(md) = std::fs::metadata(root) {
            out.push(Walked {
                path: root.to_path_buf(),
                is_dir: true,
                size: 0,
                mtime: mtime_of(&md),
            });
        }
    }
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name != ".github" {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            out.push(Walked {
                path: path.clone(),
                is_dir: true,
                size: 0,
                mtime: mtime_of(&md),
            });
            walk_inner(root, &path, depth + 1, max_depth, out);
        } else if md.is_file() {
            out.push(Walked {
                path,
                is_dir: false,
                size: md.len(),
                mtime: mtime_of(&md),
            });
        }
    }
}

pub(crate) fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn cheap_hash(path: &str, size: u64, mtime: i64) -> String {
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(size.to_le_bytes());
    h.update(mtime.to_le_bytes());
    format!("{:x}", h.finalize())
}

fn expand_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb.to_string_lossy().to_string()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(pb).to_string_lossy().to_string()
    } else {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposer_finds_groups_units_and_product() {
        let base = PathBuf::from("/x/hireflow360-platform");
        let apps = base.join("apps");
        let services = base.join("services");
        let paths = vec![
            base.clone(),
            apps.clone(),
            apps.join("app-profilex"),
            apps.join("website-profilex"),
            apps.join("app-hireflow360"),
            services.clone(),
            services.join("profilex-api"),
            services.join("auth-api"),
            apps.join("app-profilex").join("main.rs"),
        ];
        // Mark every path except files as a directory for the test.
        let dirs: HashSet<PathBuf> = paths
            .iter()
            .filter(|p| p.extension().is_none())
            .cloned()
            .collect();
        let cands = propose(&paths, &dirs, &[]);
        let kinds: Vec<&str> = cands.iter().map(|c| c.kind.as_str()).collect();
        assert!(kinds.contains(&"group_entity"));
        assert!(kinds.contains(&"unit_entity"));
        assert!(kinds.contains(&"product_entity"));
        let mem = cands
            .iter()
            .filter(|c| c.kind == "membership")
            .filter(|c| c.payload["entity_key"] == "profilex")
            .count();
        assert!(mem >= 3, "profilex should span 3 units, got {mem}");
        assert!(cands.iter().any(|c| c.kind == "facet_assign"
            && c.payload["dimension"] == "modality"
            && c.payload["value"] == "code"));
    }

    /// DB-backed corpus-scoping regression test. Runs only when
    /// `FF_BRAIN_TEST_PG` holds a Postgres URL (e.g. the fleet brain DB);
    /// otherwise it's a silent skip so plain `cargo test` stays green offline.
    ///
    /// Covers the `ff cortex index --slug <new>` files=0 bug: a directory
    /// already registered as a source of corpus A must be re-owned (source
    /// corpus_id + node project) when scanned into corpus B, and stale
    /// out-of-root content rows must be invalidated by scan().
    #[tokio::test]
    async fn scan_reassigns_dir_to_new_corpus_and_prunes_residue() {
        let Ok(url) = std::env::var("FF_BRAIN_TEST_PG") else {
            eprintln!("FF_BRAIN_TEST_PG not set — skipping DB-backed corpus-scoping test");
            return;
        };
        let pg = PgPool::connect(&url)
            .await
            .expect("connect FF_BRAIN_TEST_PG");

        let base = std::env::temp_dir().join(format!("ff-corpus-scope-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        let root = base.to_string_lossy().to_string();

        let suffix = &Uuid::new_v4().simple().to_string()[..8];
        let slug_a = format!("cscope-test-a-{suffix}");
        let slug_b = format!("cscope-test-b-{suffix}");

        let current_files = |slug: String| {
            let pg = pg.clone();
            async move {
                let n: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM brain_vault_nodes
                      WHERE project = $1 AND node_type = 'content:file'
                        AND valid_until IS NULL",
                )
                .bind(&slug)
                .fetch_one(&pg)
                .await
                .unwrap();
                n
            }
        };

        // Scan the dir into corpus A.
        let a = add_corpus(&pg, &slug_a, &slug_a, &[(root.clone(), None)])
            .await
            .unwrap();
        let ra = scan(&pg, &a, None, 4).await.unwrap();
        assert!(ra.files >= 1, "corpus A should scan at least 1 file");
        assert!(current_files(slug_a.clone()).await >= 1);

        // Same dir, NEW slug. Before the fix the source row silently stayed
        // owned by A (uq_sources_root + no corpus_id reassignment), so B's
        // scan walked zero files.
        let b = add_corpus(&pg, &slug_b, &slug_b, &[(root.clone(), None)])
            .await
            .unwrap();
        let rb = scan(&pg, &b, None, 4).await.unwrap();
        assert!(
            rb.files >= 1,
            "corpus B must own the dir after re-registration (files=0 regression)"
        );
        assert!(current_files(slug_b.clone()).await >= 1);
        assert_eq!(
            current_files(slug_a.clone()).await,
            0,
            "content is single-owner: A must no longer hold current rows for the path"
        );

        // Residue prune: plant a stale content:file in B pointing OUTSIDE its
        // root (simulates a dir mis-scanned into the slug earlier), rescan.
        let stray = format!("/nonexistent/ff-corpus-scope-stray-{suffix}/x.rs");
        sqlx::query(
            "INSERT INTO brain_vault_nodes (path, title, node_type, project, content_hash)
             VALUES ($1, 'stray', 'content:file', $2, 'x')
             ON CONFLICT (path) DO UPDATE SET project = EXCLUDED.project, valid_until = NULL",
        )
        .bind(&stray)
        .bind(&slug_b)
        .execute(&pg)
        .await
        .unwrap();
        let rb2 = scan(&pg, &b, None, 4).await.unwrap();
        assert!(rb2.pruned >= 1, "scan must invalidate out-of-root residue");
        let stray_current: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM brain_vault_nodes WHERE path = $1 AND valid_until IS NULL",
        )
        .bind(&stray)
        .fetch_optional(&pg)
        .await
        .unwrap();
        assert!(stray_current.is_none(), "stray row must be invalidated");

        // Cleanup: delete_corpus removes nodes by project (incl. the stray)
        // and cascades sources/facets/candidates off the corpus row.
        delete_corpus(&pg, &slug_a).await.unwrap();
        delete_corpus(&pg, &slug_b).await.unwrap();
        std::fs::remove_dir_all(&base).ok();
    }
}
