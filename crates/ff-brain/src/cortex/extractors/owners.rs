use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::{Context, Result};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct OwnersExtractor;

#[derive(Debug, Clone)]
struct FileNode {
    path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnerSummary {
    pub name: String,
    pub corpus: String,
    pub file_count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnedFile {
    pub path: String,
    pub title: String,
    pub corpus: String,
    pub confidence: f32,
}

#[async_trait::async_trait]
impl Extractor for OwnersExtractor {
    fn name(&self) -> &'static str {
        "owners"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            if !is_git_repo(root) {
                continue;
            }

            let tracked = tracked_source_files(root)?;
            if tracked.is_empty() {
                continue;
            }
            let file_nodes = content_file_nodes(ctx.pool, ctx.corpus_slug, root).await?;

            for rel_path in tracked {
                let Some(file_node) = file_nodes.get(&rel_path) else {
                    continue;
                };
                let Some(author) = top_author(root, &rel_path)? else {
                    continue;
                };
                facts.add_ownership(author, file_node.path.clone(), rel_path);
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    people: BTreeMap<String, f32>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            people: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_ownership(&mut self, author: String, file_path: String, rel_path: String) {
        self.people.entry(author.clone()).or_insert(0.9);

        let src_path = person_path(self.corpus, &author);
        let key = (src_path.clone(), file_path.clone(), "owns".to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path: file_path,
            edge_type: "owns".to_string(),
            confidence: 0.9,
            provenance: "git_blame".to_string(),
            method: Some("top_commits_touching_file".to_string()),
            evidence: Some(json!({
                "rel_path": rel_path,
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for (author, confidence) in self.people {
            out.push(Fact::Node {
                path: person_path(self.corpus, &author),
                title: author,
                node_type: "person:dev".to_string(),
                start_line: None,
                end_line: None,
                confidence,
                provenance: "git_blame".to_string(),
            });
        }
        out.extend(self.edges);
        out
    }
}

async fn content_file_nodes(
    pool: &PgPool,
    corpus_slug: &str,
    root: &Path,
) -> Result<HashMap<String, FileNode>> {
    let root_abs = root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", root.display()))?;
    let rows = sqlx::query(
        r#"SELECT path, title
             FROM brain_vault_nodes
            WHERE project = $1
              AND valid_until IS NULL
              AND node_type = 'content:file'"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let mut out = HashMap::new();
    for row in rows {
        let path: String = row.get("path");
        let abs = PathBuf::from(&path);
        let Ok(rel) = abs.strip_prefix(&root_abs) else {
            continue;
        };
        let rel_path = normalize_rel_path(rel);
        if rel_path.is_empty() || !is_source_path(&rel_path) {
            continue;
        }
        out.insert(rel_path, FileNode { path });
    }
    Ok(out)
}

fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .output()
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
        .unwrap_or(false)
}

fn tracked_source_files(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .output()
        .with_context(|| format!("git ls-files in {}", root.display()))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let mut files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|p| is_source_path(p))
        .map(|p| p.to_string())
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    Ok(files)
}

fn top_author(root: &Path, rel_path: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("log")
        .arg("--format=%an")
        .arg("--")
        .arg(rel_path)
        .output()
        .with_context(|| format!("git log for {}", root.join(rel_path).display()))?;
    if !output.status.success() {
        return Ok(None);
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for author in String::from_utf8_lossy(&output.stdout).lines() {
        let author = author.trim();
        if !author.is_empty() {
            *counts.entry(author.to_string()).or_insert(0) += 1;
        }
    }
    Ok(counts
        .into_iter()
        .max_by(|(a_name, a_count), (b_name, b_count)| {
            a_count.cmp(b_count).then_with(|| b_name.cmp(a_name))
        })
        .map(|(author, _)| author))
}

fn is_source_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    matches!(
        Path::new(&path).extension().and_then(|e| e.to_str()),
        Some("rs" | "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "java" | "py")
    )
}

fn normalize_rel_path(path: &Path) -> String {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

fn person_path(corpus: &str, author: &str) -> String {
    format!("person://{corpus}/{author}")
}

pub async fn owners(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<OwnerSummary>> {
    let rows = sqlx::query(
        r#"SELECT p.title AS name,
                  p.project AS corpus,
                  COUNT(DISTINCT e.dst_id) AS file_count
             FROM brain_vault_nodes p
             JOIN brain_vault_edges e
               ON e.src_id = p.id
              AND e.edge_type = 'owns'
             JOIN brain_vault_nodes f
               ON f.id = e.dst_id
              AND f.node_type = 'content:file'
            WHERE p.node_type = 'person:dev'
              AND ($1::text IS NULL OR p.project = $1)
              AND COALESCE(p.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = p.project), 0)
              )
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = p.project), 0)
              )
            GROUP BY p.title, p.project
            ORDER BY file_count DESC, p.project, p.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| OwnerSummary {
            name: r.get("name"),
            corpus: r.get("corpus"),
            file_count: r.get("file_count"),
        })
        .collect())
}

pub async fn owner_files(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    name: &str,
) -> Result<Vec<OwnedFile>> {
    let rows = sqlx::query(
        r#"SELECT f.path,
                  f.title,
                  f.project AS corpus,
                  e.confidence
             FROM brain_vault_nodes p
             JOIN brain_vault_edges e
               ON e.src_id = p.id
              AND e.edge_type = 'owns'
             JOIN brain_vault_nodes f
               ON f.id = e.dst_id
              AND f.node_type = 'content:file'
            WHERE p.node_type = 'person:dev'
              AND p.title = $1
              AND ($2::text IS NULL OR p.project = $2)
              AND COALESCE(p.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = p.project), 0)
              )
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = p.project), 0)
              )
            ORDER BY f.project, f.path"#,
    )
    .bind(name)
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| OwnedFile {
            path: r.get("path"),
            title: r.get("title"),
            corpus: r.get("corpus"),
            confidence: r.get("confidence"),
        })
        .collect())
}
