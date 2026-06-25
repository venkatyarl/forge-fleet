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

/// One dependency edge out of a specific crate (forward direction).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrateDepEdge {
    pub name: String,
    pub version: Option<String>,
    pub section: Option<String>,
    pub ecosystem: Option<String>,
    /// True when a `code:crate` node with this name exists in the corpus — i.e.
    /// a workspace-internal dependency rather than a third-party package.
    pub internal: bool,
}

/// One crate that depends ON the queried crate (reverse direction).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrateDependent {
    pub name: String,
    pub node_type: String,
}

/// Forward (what it needs) + reverse (what needs it) dependency view for a
/// single crate. The reverse list is the workspace rebuild "blast radius".
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CrateDeps {
    pub crate_name: String,
    pub corpus: Option<String>,
    pub dependencies: Vec<CrateDepEdge>,
    pub dependents: Vec<CrateDependent>,
}

// Generation-liveness predicate shared by both per-crate queries: a row is live
// when its generation is 0 (base) or the corpus's current generation. Mirrors
// the filter in `deps()` so per-crate queries can't surface stale-generation rows.
const GEN_LIVE: &str = "COALESCE({alias}.generation, 0) IN (
    0, COALESCE((SELECT current_generation FROM cortex_generations
                  WHERE project = {proj}), 0))";

/// Per-crate dependency graph: what `crate_name` depends on (forward) and what
/// depends on it (reverse). `corpus_slug = None` searches every indexed corpus.
pub async fn deps_for_crate(
    pool: &PgPool,
    crate_name: &str,
    corpus_slug: Option<&str>,
) -> Result<CrateDeps> {
    let fwd_gen_c = GEN_LIVE
        .replace("{alias}", "c")
        .replace("{proj}", "c.project");
    let fwd_gen_e = GEN_LIVE
        .replace("{alias}", "e")
        .replace("{proj}", "c.project");
    let fwd_gen_d = GEN_LIVE
        .replace("{alias}", "d")
        .replace("{proj}", "c.project");
    let fwd_sql = format!(
        r#"SELECT d.title AS package,
                  e.evidence->>'version'   AS version,
                  e.evidence->>'section'   AS section,
                  e.evidence->>'ecosystem' AS ecosystem,
                  EXISTS (SELECT 1 FROM brain_vault_nodes ic
                           WHERE ic.node_type = 'code:crate'
                             AND ic.title = d.title
                             AND ic.project = c.project) AS internal
             FROM brain_vault_nodes c
             JOIN brain_vault_edges e ON e.src_id = c.id AND e.edge_type = 'depends_on'
             JOIN brain_vault_nodes d ON d.id = e.dst_id
            WHERE c.node_type = 'code:crate'
              AND c.title = $1
              AND ($2::text IS NULL OR c.project = $2)
              AND {fwd_gen_c} AND {fwd_gen_e} AND {fwd_gen_d}
            ORDER BY internal DESC, d.title"#
    );
    let dependencies = sqlx::query(&fwd_sql)
        .bind(crate_name)
        .bind(corpus_slug)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| CrateDepEdge {
            name: r.get("package"),
            version: r.try_get("version").ok(),
            section: r.try_get("section").ok(),
            ecosystem: r.try_get("ecosystem").ok(),
            internal: r.try_get("internal").unwrap_or(false),
        })
        .collect();

    let rev_gen_p = GEN_LIVE
        .replace("{alias}", "p")
        .replace("{proj}", "p.project");
    let rev_gen_e = GEN_LIVE
        .replace("{alias}", "e")
        .replace("{proj}", "p.project");
    let rev_gen_c = GEN_LIVE
        .replace("{alias}", "c")
        .replace("{proj}", "p.project");
    let rev_sql = format!(
        r#"SELECT DISTINCT c.title AS dependent, c.node_type AS node_type
             FROM brain_vault_nodes p
             JOIN brain_vault_edges e ON e.dst_id = p.id AND e.edge_type = 'depends_on'
             JOIN brain_vault_nodes c ON c.id = e.src_id
            WHERE p.title = $1
              AND p.node_type IN ('dep:package', 'code:crate')
              AND c.node_type IN ('code:crate', 'code:mod')
              AND ($2::text IS NULL OR p.project = $2)
              AND {rev_gen_p} AND {rev_gen_e} AND {rev_gen_c}
            ORDER BY c.title"#
    );
    let dependents = sqlx::query(&rev_sql)
        .bind(crate_name)
        .bind(corpus_slug)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| CrateDependent {
            name: r.get("dependent"),
            node_type: r.get("node_type"),
        })
        .collect();

    Ok(CrateDeps {
        crate_name: crate_name.to_string(),
        corpus: corpus_slug.map(ToString::to_string),
        dependencies,
        dependents,
    })
}

/// Render a [`CrateDeps`] as a human-readable report (pure; used by the CLI).
pub fn render_crate_deps(cd: &CrateDeps) -> String {
    let mut out = String::new();
    let scope = cd.corpus.as_deref().unwrap_or("all corpora");
    out.push_str(&format!("crate '{}' ({scope})\n", cd.crate_name));

    let (internal, external): (Vec<_>, Vec<_>) = cd.dependencies.iter().partition(|d| d.internal);
    out.push_str(&format!(
        "\n  depends on ({} internal, {} external):\n",
        internal.len(),
        external.len()
    ));
    if cd.dependencies.is_empty() {
        out.push_str("    (none — crate not found, or has no manifest deps)\n");
    }
    for d in internal.iter().chain(external.iter()) {
        let tag = if d.internal { "crate" } else { "pkg" };
        let ver = d.version.as_deref().unwrap_or("*");
        out.push_str(&format!("    → [{tag}] {} {ver}\n", d.name));
    }

    out.push_str(&format!(
        "\n  depended on by ({} — rebuild blast radius):\n",
        cd.dependents.len()
    ));
    if cd.dependents.is_empty() {
        out.push_str("    (none)\n");
    }
    for c in &cd.dependents {
        out.push_str(&format!("    ← {}\n", c.name));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(name: &str, ver: &str, internal: bool) -> CrateDepEdge {
        CrateDepEdge {
            name: name.into(),
            version: Some(ver.into()),
            section: Some("dependencies".into()),
            ecosystem: Some("cargo".into()),
            internal,
        }
    }

    #[test]
    fn render_groups_internal_first_and_lists_blast_radius() {
        let cd = CrateDeps {
            crate_name: "ff-agent".into(),
            corpus: Some("forge-fleet".into()),
            dependencies: vec![
                edge("tokio", "1", false),
                edge("ff-db", "*", true),
                edge("ff-core", "*", true),
            ],
            dependents: vec![
                CrateDependent {
                    name: "ff-terminal".into(),
                    node_type: "code:crate".into(),
                },
                CrateDependent {
                    name: "forge-fleet".into(),
                    node_type: "code:crate".into(),
                },
            ],
        };
        let out = render_crate_deps(&cd);
        assert!(out.contains("crate 'ff-agent' (forge-fleet)"));
        assert!(out.contains("2 internal, 1 external"));
        // Internal deps render before external ones.
        let ffdb = out.find("ff-db").unwrap();
        let tokio = out.find("tokio").unwrap();
        assert!(
            ffdb < tokio,
            "internal crate deps must list before external pkgs"
        );
        assert!(out.contains("[crate] ff-db"));
        assert!(out.contains("[pkg] tokio"));
        assert!(out.contains("blast radius"));
        assert!(out.contains("← ff-terminal"));
    }

    #[test]
    fn render_handles_empty_both_directions() {
        let cd = CrateDeps {
            crate_name: "ghost".into(),
            corpus: None,
            ..Default::default()
        };
        let out = render_crate_deps(&cd);
        assert!(out.contains("ghost' (all corpora)"));
        assert!(out.contains("crate not found"));
        assert!(out.contains("(none)"));
    }
}
