use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const NODE_TYPE: &str = "product:feature";
const EDGE_TYPE: &str = "implements";
const PROVENANCE: &str = "ast";
const CONFIDENCE: f32 = 0.6;

pub struct ProductExtractor;

#[derive(Debug, Clone)]
struct Feature {
    enum_name: String,
    variant_name: String,
    docs: Vec<String>,
}

#[derive(Debug, Clone)]
struct CodeFunction {
    path: String,
    title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureRow {
    pub corpus: String,
    pub path: String,
    pub feature: String,
    pub implements: Option<String>,
}

impl FeatureRow {
    pub fn has_implements(&self) -> bool {
        self.implements.is_some()
    }
}

#[async_trait::async_trait]
impl Extractor for ProductExtractor {
    fn name(&self) -> &'static str {
        "product"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut features_by_path = BTreeMap::new();
        for root in &ctx.roots {
            for path in collect_files(root, "rs")? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                for feature in extract_features_from_source(&source) {
                    features_by_path
                        .entry(feature_path(ctx.corpus_slug, &feature))
                        .or_insert(feature);
                }
            }
        }

        if features_by_path.is_empty() {
            return Ok(Vec::new());
        }

        let functions = load_code_functions(ctx.pool, ctx.corpus_slug).await?;
        let resolver = FunctionResolver::new(functions);

        for (path, feature) in features_by_path {
            let feature_id =
                upsert_feature_node(ctx.pool, &path, ctx.corpus_slug, &feature, ctx.generation)
                    .await?;
            if let Some(function) = resolver.resolve(&to_snake_case(&feature.variant_name)) {
                add_edge(
                    ctx.pool,
                    feature_id,
                    &function.path,
                    ctx.generation,
                    &feature,
                    function,
                )
                .await?;
            }
        }

        Ok(Vec::new())
    }
}

pub async fn features(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<FeatureRow>> {
    let rows = sqlx::query(
        r#"
        SELECT n.project AS corpus,
               n.path,
               n.title AS feature,
               impl.title AS implements
          FROM brain_vault_nodes n
          LEFT JOIN LATERAL (
                SELECT dst.title
                  FROM brain_vault_edges e
                  JOIN brain_vault_nodes dst ON dst.id = e.dst_id
                 WHERE e.src_id = n.id
                   AND e.edge_type = $2
                   AND dst.valid_until IS NULL
                 ORDER BY dst.title
                 LIMIT 1
          ) impl ON TRUE
         WHERE n.node_type = $1
           AND n.valid_until IS NULL
           AND ($3::text IS NULL OR n.project = $3)
         ORDER BY n.project, n.title
        "#,
    )
    .bind(NODE_TYPE)
    .bind(EDGE_TYPE)
    .bind(corpus)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| FeatureRow {
            corpus: row.get("corpus"),
            path: row.get("path"),
            feature: row.get("feature"),
            implements: row.get("implements"),
        })
        .collect())
}

async fn upsert_feature_node(
    pool: &PgPool,
    path: &str,
    corpus: &str,
    feature: &Feature,
    generation: i64,
) -> Result<Uuid> {
    let id = sqlx::query_scalar(
        r#"
        INSERT INTO brain_vault_nodes
            (path, title, node_type, project, content_hash, generation, confidence, provenance)
        VALUES ($1, $2, $3, $4, $1, $5, $6, $7)
        ON CONFLICT (path) DO UPDATE
          SET title = EXCLUDED.title,
              node_type = EXCLUDED.node_type,
              project = EXCLUDED.project,
              valid_until = NULL,
              updated_at = NOW(),
              generation = EXCLUDED.generation,
              confidence = GREATEST(brain_vault_nodes.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance
        RETURNING id
        "#,
    )
    .bind(path)
    .bind(&feature.variant_name)
    .bind(NODE_TYPE)
    .bind(corpus)
    .bind(generation)
    .bind(CONFIDENCE)
    .bind(PROVENANCE)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn add_edge(
    pool: &PgPool,
    src: Uuid,
    dst_path: &str,
    generation: i64,
    feature: &Feature,
    function: &CodeFunction,
) -> Result<bool> {
    let inserted = sqlx::query_scalar(
        r#"
        WITH dst AS (
            SELECT id
              FROM brain_vault_nodes
             WHERE path = $2
             LIMIT 1
        )
        INSERT INTO brain_vault_edges
            (src_id, dst_id, edge_type, provenance, confidence, method, evidence, generation)
        SELECT $1, id, $3, $4, $5, 'EXTRACTED', $6, $7
          FROM dst
        ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
          SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
              provenance = EXCLUDED.provenance,
              method = COALESCE(EXCLUDED.method, brain_vault_edges.method),
              evidence = COALESCE(EXCLUDED.evidence, brain_vault_edges.evidence),
              generation = EXCLUDED.generation
        RETURNING (xmax = 0) AS inserted
        "#,
    )
    .bind(src)
    .bind(dst_path)
    .bind(EDGE_TYPE)
    .bind(PROVENANCE)
    .bind(CONFIDENCE)
    .bind(json!({
        "enum": feature.enum_name,
        "variant": feature.variant_name,
        "doc_comment": feature.docs.join("\n"),
        "resolver": "variant_snake_case_function_name",
        "function": function.title,
    }))
    .bind(generation)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.unwrap_or(false))
}

async fn load_code_functions(pool: &PgPool, corpus: &str) -> Result<Vec<CodeFunction>> {
    // Deliberately no current_generation filter: product runs in the same indexing
    // pass after code symbols are written, before the generation is published.
    let rows = sqlx::query(
        r#"
        SELECT path, title
          FROM brain_vault_nodes
         WHERE project = $1
           AND node_type = 'code:function'
           AND valid_until IS NULL
         ORDER BY title
        "#,
    )
    .bind(corpus)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CodeFunction {
            path: row.get("path"),
            title: row.get("title"),
        })
        .collect())
}

struct FunctionResolver {
    by_leaf: HashMap<String, Vec<CodeFunction>>,
}

impl FunctionResolver {
    fn new(functions: Vec<CodeFunction>) -> Self {
        let mut by_leaf: HashMap<String, Vec<CodeFunction>> = HashMap::new();
        for function in functions {
            by_leaf
                .entry(leaf(&function.title).to_string())
                .or_default()
                .push(function);
        }
        for functions in by_leaf.values_mut() {
            functions.sort_by_key(|f| (f.title.len(), f.title.clone()));
        }
        Self { by_leaf }
    }

    fn resolve(&self, name: &str) -> Option<&CodeFunction> {
        self.by_leaf.get(name).and_then(|matches| matches.first())
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

fn extract_features_from_source(source: &str) -> Vec<Feature> {
    let enum_re = Regex::new(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)").expect("valid enum regex");
    let variant_re =
        Regex::new(r"^([A-Z][A-Za-z0-9_]*)(?:\s|,|\(|\{|\=|$)").expect("valid variant regex");
    let mut out = Vec::new();
    let mut pending_attrs = Vec::new();
    let mut enum_state: Option<EnumState> = None;

    for line in source.lines() {
        if let Some(state) = enum_state.as_mut() {
            let trim = line.trim();
            if state.depth == 1 {
                if let Some(doc) = doc_comment(trim) {
                    state.pending_docs.push(doc.to_string());
                    state.depth += brace_delta(line);
                    if state.depth <= 0 {
                        enum_state = None;
                    }
                    continue;
                }
                if trim.starts_with("#[") {
                    state.pending_attrs.push(trim.to_string());
                    state.depth += brace_delta(line);
                    if state.depth <= 0 {
                        enum_state = None;
                    }
                    continue;
                }
                if let Some(caps) = variant_re.captures(trim) {
                    if let Some(name) = caps.get(1).map(|m| m.as_str().to_string()) {
                        out.push(Feature {
                            enum_name: state.name.clone(),
                            variant_name: name,
                            docs: std::mem::take(&mut state.pending_docs),
                        });
                        state.pending_attrs.clear();
                    }
                } else if !trim.is_empty() && !trim.starts_with("//") {
                    state.pending_docs.clear();
                    state.pending_attrs.clear();
                }
            }
            state.depth += brace_delta(line);
            if state.depth <= 0 {
                enum_state = None;
            }
            continue;
        }

        let trim = line.trim();
        if trim.starts_with("#[") {
            pending_attrs.push(trim.to_string());
            continue;
        }
        if let Some(caps) = enum_re.captures(line) {
            let attrs = pending_attrs.join("\n");
            pending_attrs.clear();
            if !attrs.contains("Subcommand") {
                continue;
            }
            let Some(name) = caps.get(1).map(|m| m.as_str().to_string()) else {
                continue;
            };
            let depth = brace_delta(line);
            if depth > 0 {
                enum_state = Some(EnumState {
                    name,
                    depth,
                    pending_docs: Vec::new(),
                    pending_attrs: Vec::new(),
                });
            }
            continue;
        }
        if !trim.is_empty() && !trim.starts_with("///") {
            pending_attrs.clear();
        }
    }

    out
}

#[derive(Debug)]
struct EnumState {
    name: String,
    depth: i32,
    pending_docs: Vec<String>,
    pending_attrs: Vec<String>,
}

fn doc_comment(trim: &str) -> Option<&str> {
    trim.strip_prefix("///").map(str::trim)
}

fn brace_delta(line: &str) -> i32 {
    line.chars().fold(0, |acc, ch| match ch {
        '{' => acc + 1,
        '}' => acc - 1,
        _ => acc,
    })
}

fn feature_path(corpus: &str, feature: &Feature) -> String {
    format!(
        "product://{}/feature/{}::{}",
        corpus, feature.enum_name, feature.variant_name
    )
}

fn leaf(title: &str) -> &str {
    title.rsplit("::").next().unwrap_or(title)
}

fn to_snake_case(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower_or_digit {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower_or_digit = false;
        } else {
            out.push(ch);
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_variants_from_subcommand_enum() {
        let source = r#"
            #[derive(Debug, Clone, Subcommand)]
            enum CortexCommand {
                /// Parse a corpus.
                Index { slug: String },
                #[command(visible_alias = "owner")]
                Owners,
            }
        "#;
        let features = extract_features_from_source(source);
        assert_eq!(features.len(), 2);
        assert_eq!(features[0].variant_name, "Index");
        assert_eq!(features[0].docs, vec!["Parse a corpus."]);
        assert_eq!(features[1].variant_name, "Owners");
    }

    #[test]
    fn converts_variant_name_to_snake_case() {
        assert_eq!(to_snake_case("IngestPm"), "ingest_pm");
        assert_eq!(to_snake_case("Callers"), "callers");
    }
}
