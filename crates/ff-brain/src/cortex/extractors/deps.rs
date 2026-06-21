use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

pub struct DepsExtractor;

#[derive(Debug, Clone)]
struct ManifestDep {
    package: String,
    alias: String,
    version: Option<String>,
    section: &'static str,
    manifest: String,
    ecosystem: &'static str,
}

#[derive(Debug, Clone)]
struct Manifest {
    crate_name: String,
    deps: Vec<ManifestDep>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DepPackageSummary {
    pub package: String,
    pub corpus: String,
    pub dependent_count: i64,
    pub versions: Vec<String>,
}

#[async_trait::async_trait]
impl Extractor for DepsExtractor {
    fn name(&self) -> &'static str {
        "deps"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_manifest_files(root)? {
                let Some(manifest) = parse_manifest(&path)? else {
                    continue;
                };
                facts.add_manifest(manifest);
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    crates: BTreeSet<String>,
    packages: BTreeSet<String>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            crates: BTreeSet::new(),
            packages: BTreeSet::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_manifest(&mut self, manifest: Manifest) {
        self.crates.insert(manifest.crate_name.clone());
        for dep in manifest.deps {
            self.packages.insert(dep.package.clone());
            let src_path = crate_path(self.corpus, &manifest.crate_name);
            let dst_path = package_path(self.corpus, &dep.package);
            let key = (src_path.clone(), dst_path.clone(), "depends_on".to_string());
            if !self.seen_edges.insert(key) {
                continue;
            }
            self.edges.push(Fact::Edge {
                src_path,
                dst_path,
                edge_type: "depends_on".to_string(),
                confidence: 1.0,
                provenance: "manifest".to_string(),
                method: Some("EXTRACTED".to_string()),
                evidence: Some(json!({
                    "version": dep.version,
                    "section": dep.section,
                    "manifest": dep.manifest,
                    "ecosystem": dep.ecosystem,
                    "alias": dep.alias,
                })),
            });
        }
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for crate_name in &self.crates {
            out.push(Fact::Node {
                path: crate_path(self.corpus, crate_name),
                title: crate_name.clone(),
                node_type: "code:crate".to_string(),
                start_line: None,
                end_line: None,
                confidence: 1.0,
                provenance: "manifest".to_string(),
            });
        }
        for package in &self.packages {
            out.push(Fact::Node {
                path: package_path(self.corpus, package),
                title: package.clone(),
                node_type: "dep:package".to_string(),
                start_line: None,
                end_line: None,
                confidence: 1.0,
                provenance: "manifest".to_string(),
            });
        }
        out.extend(self.edges);
        out
    }
}

pub async fn deps(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<DepPackageSummary>> {
    let rows = sqlx::query(
        r#"SELECT p.title AS package,
                  p.project AS corpus,
                  COUNT(DISTINCT e.src_id) AS dependent_count,
                  ARRAY_REMOVE(ARRAY_AGG(DISTINCT e.evidence->>'version'), NULL) AS versions
             FROM brain_vault_nodes p
             JOIN brain_vault_edges e
               ON e.dst_id = p.id
              AND e.edge_type = 'depends_on'
             JOIN brain_vault_nodes c
               ON c.id = e.src_id
            WHERE p.node_type = 'dep:package'
              AND c.node_type IN ('code:crate', 'code:mod')
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
            ORDER BY dependent_count DESC, p.project, p.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| DepPackageSummary {
            package: r.get("package"),
            corpus: r.get("corpus"),
            dependent_count: r.get("dependent_count"),
            versions: r
                .try_get::<Vec<String>, _>("versions")
                .unwrap_or_else(|_| Vec::new()),
        })
        .collect())
}

fn parse_manifest(path: &Path) -> Result<Option<Manifest>> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml") => parse_cargo_manifest(path),
        Some("package.json") => parse_package_json(path),
        _ => Ok(None),
    }
}

fn parse_cargo_manifest(path: &Path) -> Result<Option<Manifest>> {
    let raw = fs::read_to_string(path)?;
    let value: TomlValue = raw.parse()?;
    let Some(package) = value.get("package").and_then(|v| v.as_table()) else {
        return Ok(None);
    };
    let Some(crate_name) = package.get("name").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let manifest_path = path.to_string_lossy().to_string();
    let mut deps = Vec::new();
    for (section, table_name) in [
        ("dependencies", "dependencies"),
        ("dev-dependencies", "dev-dependencies"),
        ("build-dependencies", "build-dependencies"),
    ] {
        let Some(table) = value.get(table_name).and_then(|v| v.as_table()) else {
            continue;
        };
        for (alias, spec) in table {
            deps.push(ManifestDep {
                package: cargo_package_name(alias, spec),
                alias: alias.clone(),
                version: cargo_version(spec),
                section,
                manifest: manifest_path.clone(),
                ecosystem: "cargo",
            });
        }
    }
    Ok(Some(Manifest {
        crate_name: crate_name.to_string(),
        deps,
    }))
}

fn parse_package_json(path: &Path) -> Result<Option<Manifest>> {
    let raw = fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    let crate_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "package".to_string());
    let manifest_path = path.to_string_lossy().to_string();
    let mut deps = Vec::new();
    for (section, key) in [
        ("dependencies", "dependencies"),
        ("devDependencies", "devDependencies"),
    ] {
        let Some(obj) = value.get(key).and_then(|v| v.as_object()) else {
            continue;
        };
        for (package, spec) in obj {
            deps.push(ManifestDep {
                package: package.clone(),
                alias: package.clone(),
                version: spec.as_str().map(ToString::to_string),
                section,
                manifest: manifest_path.clone(),
                ecosystem: "npm",
            });
        }
    }
    Ok(Some(Manifest { crate_name, deps }))
}

fn cargo_package_name(alias: &str, spec: &TomlValue) -> String {
    spec.as_table()
        .and_then(|table| table.get("package"))
        .and_then(|v| v.as_str())
        .unwrap_or(alias)
        .to_string()
}

fn cargo_version(spec: &TomlValue) -> Option<String> {
    if let Some(s) = spec.as_str() {
        return Some(s.to_string());
    }
    spec.as_table()
        .and_then(|table| table.get("version"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn collect_manifest_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_manifest_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_manifest_files_inner(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        if matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("Cargo.toml" | "package.json")
        ) {
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
    if matches!(
        name,
        ".git" | "target" | "node_modules" | ".direnv" | "dist" | "build"
    ) {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        collect_manifest_files_inner(&entry?.path(), out)?;
    }
    Ok(())
}

fn crate_path(corpus: &str, crate_name: &str) -> String {
    format!("crate://{corpus}/{crate_name}")
}

fn package_path(corpus: &str, package: &str) -> String {
    format!("dep://{corpus}/{package}")
}
