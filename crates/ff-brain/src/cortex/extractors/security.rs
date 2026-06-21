use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::{Result, anyhow};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct SecurityExtractor;

#[derive(Debug, Clone)]
struct GateSite {
    gate: &'static str,
    token: &'static str,
    method: &'static str,
    line: i32,
}

#[derive(Debug, Clone)]
struct FunctionSpan {
    path: String,
    start_line: i32,
    end_line: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityFunctionRef {
    pub qualified_name: String,
    pub path: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub confidence: Option<f32>,
    pub method: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityGateSummary {
    pub gate: String,
    pub corpus: String,
    pub protected_functions: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityGatesReport {
    pub gates: Vec<SecurityGateSummary>,
    pub unguarded_handlers: Vec<SecurityFunctionRef>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityGuardDetail {
    pub symbol: String,
    pub corpus: String,
    pub gates: Vec<SecurityGateRef>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityGateRef {
    pub gate: String,
    pub confidence: f32,
    pub method: Option<String>,
}

#[async_trait::async_trait]
impl Extractor for SecurityExtractor {
    fn name(&self) -> &'static str {
        "security"
    }

    async fn extract(&self, ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        let mut facts = FactBuilder::new(ctx.corpus_slug);

        for root in &ctx.roots {
            for path in collect_files(root, "rs")? {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let file_path = path.to_string_lossy().to_string();
                let functions = function_spans(ctx, &file_path).await?;
                if functions.is_empty() {
                    continue;
                }

                for site in find_gate_sites(&source) {
                    let Some(function) = enclosing_function(&functions, site.line) else {
                        continue;
                    };
                    facts.add_guard(function.path.clone(), site);
                }
            }
        }

        Ok(facts.finish())
    }
}

struct FactBuilder<'a> {
    corpus: &'a str,
    gates: BTreeMap<String, f32>,
    edges: Vec<Fact>,
    seen_edges: HashSet<(String, String, String)>,
}

impl<'a> FactBuilder<'a> {
    fn new(corpus: &'a str) -> Self {
        Self {
            corpus,
            gates: BTreeMap::new(),
            edges: Vec::new(),
            seen_edges: HashSet::new(),
        }
    }

    fn add_guard(&mut self, src_path: String, site: GateSite) {
        self.gates
            .entry(site.gate.to_string())
            .and_modify(|confidence| *confidence = confidence.max(0.8))
            .or_insert(0.8);

        let dst_path = gate_path(self.corpus, site.gate);
        let key = (src_path.clone(), dst_path.clone(), "guarded_by".to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Fact::Edge {
            src_path,
            dst_path,
            edge_type: "guarded_by".to_string(),
            confidence: 0.7,
            provenance: "ast".to_string(),
            method: Some(site.method.to_string()),
            evidence: Some(json!({
                "line": site.line,
                "token": site.token,
                "gate": site.gate,
            })),
        });
    }

    fn finish(self) -> Vec<Fact> {
        let mut out = Vec::new();
        for (gate, confidence) in &self.gates {
            out.push(Fact::Node {
                path: gate_path(self.corpus, gate),
                title: gate.clone(),
                node_type: "security:gate".to_string(),
                start_line: None,
                end_line: None,
                confidence: *confidence,
                provenance: "ast".to_string(),
            });
        }
        out.extend(self.edges);
        out
    }
}

async fn function_spans(ctx: &ExtractCtx<'_>, file_path: &str) -> Result<Vec<FunctionSpan>> {
    let rows = sqlx::query(
        r#"WITH RECURSIVE down(id) AS (
               SELECT dst_id
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes f ON f.id = e.src_id
                WHERE f.project = $1
                  AND f.node_type = 'content:file'
                  AND f.path = $2
                  AND e.edge_type = 'contains'
               UNION
               SELECT e.dst_id
                 FROM brain_vault_edges e
                 JOIN down d ON d.id = e.src_id
                WHERE e.edge_type = 'contains'
           )
           SELECT n.path, n.start_line, n.end_line
             FROM down
             JOIN brain_vault_nodes n ON n.id = down.id
            WHERE n.node_type = 'code:function'
              AND n.start_line IS NOT NULL
              AND n.end_line IS NOT NULL
            ORDER BY n.start_line DESC,
                     (n.end_line - n.start_line) ASC"#,
    )
    .bind(ctx.corpus_slug)
    .bind(file_path)
    .fetch_all(ctx.pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| FunctionSpan {
            path: r.get("path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
        })
        .collect())
}

fn enclosing_function(functions: &[FunctionSpan], line: i32) -> Option<&FunctionSpan> {
    functions
        .iter()
        .filter(|f| f.start_line <= line && line <= f.end_line)
        .min_by_key(|f| f.end_line - f.start_line)
}

fn find_gate_sites(source: &str) -> Vec<GateSite> {
    let line_starts = line_start_offsets(source);
    let mut sites = Vec::new();

    for pattern in gate_patterns() {
        for at in find_token_occurrences(source, pattern.token, pattern.ident) {
            if pattern.call_like {
                if is_function_definition(source, at) {
                    continue;
                }
                let Some(next) = source[at + pattern.token.len()..]
                    .chars()
                    .find(|ch| !ch.is_whitespace())
                else {
                    continue;
                };
                if next != '(' {
                    continue;
                }
            }
            sites.push(GateSite {
                gate: pattern.gate,
                token: pattern.token,
                method: pattern.method,
                line: byte_to_line(&line_starts, at),
            });
        }
    }

    sites.sort_by_key(|s| (s.line, s.gate, s.token));
    sites
}

#[derive(Debug, Clone, Copy)]
struct GatePattern {
    token: &'static str,
    gate: &'static str,
    method: &'static str,
    call_like: bool,
    ident: bool,
}

fn gate_patterns() -> Vec<GatePattern> {
    vec![
        call("require_admin", "admin-token"),
        call("verify_admin", "admin-token"),
        literal("FF_ADMIN_TOKEN", "admin-token"),
        literal("x-admin-token", "admin-token"),
        literal("X-Admin-Token", "admin-token"),
        call("verify_signature", "hmac-signature"),
        call("verify_hmac", "hmac-signature"),
        call("compute_hmac_hex", "hmac-signature"),
        call("verify_json", "hmac-signature"),
        call("is_request_fresh", "hmac-signature"),
        literal("HmacSha256", "hmac-signature"),
        literal("jsonwebtoken::decode", "jwt-claims"),
        call("verify_jwt", "jwt-claims"),
        call("validate_token", "jwt-claims"),
        literal("JwtClaims", "jwt-claims"),
        literal("FF_JWT_SECRET", "jwt-claims"),
        literal("Authorization", "jwt-claims"),
        call("required_rank", "rbac-role"),
        call("role_rank", "rbac-role"),
        call("require_role", "rbac-role"),
        call("has_permission", "rbac-role"),
        call("is_authorized", "rbac-role"),
        literal("insufficient role", "rbac-role"),
        literal("allowed_roles", "rbac-role"),
        literal("role_not_allowed", "rbac-role"),
        literal("tenant_id", "tenant-scope"),
        literal("tenant", "tenant-scope"),
        literal("FF_WEBHOOK_SECRET", "webhook-secret"),
        literal("x-webhook-secret", "webhook-secret"),
        literal("X-Webhook-Secret", "webhook-secret"),
        call("ct_eq", "webhook-secret"),
        call("extract_enrollment_token_from_headers", "enrollment-token"),
        call("resolve_shared_secret", "enrollment-token"),
        literal("EnrollmentEnforcement::Required", "enrollment-token"),
        literal("invalid enrollment token", "enrollment-token"),
    ]
}

fn call(token: &'static str, gate: &'static str) -> GatePattern {
    GatePattern {
        token,
        gate,
        method: "CALL",
        call_like: true,
        ident: true,
    }
}

fn literal(token: &'static str, gate: &'static str) -> GatePattern {
    GatePattern {
        token,
        gate,
        method: "TOKEN",
        call_like: false,
        ident: token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
    }
}

fn find_token_occurrences(source: &str, token: &str, ident: bool) -> Vec<usize> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(pos) = source[offset..].find(token) {
        let at = offset + pos;
        let boundary_ok = if ident {
            let before = source[..at].chars().next_back();
            let after = source[at + token.len()..].chars().next();
            before.is_none_or(|ch| !is_ident_char(ch)) && after.is_none_or(|ch| !is_ident_char(ch))
        } else {
            true
        };
        if boundary_ok {
            out.push(at);
        }
        offset = at + token.len();
    }
    out
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_function_definition(source: &str, at: usize) -> bool {
    let prefix = &source[..at];
    let prefix = prefix.trim_end();
    prefix.ends_with("fn") || prefix.ends_with("async fn") || prefix.ends_with("pub fn")
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn byte_to_line(line_starts: &[usize], byte: usize) -> i32 {
    line_starts.partition_point(|&s| s <= byte).max(1) as i32
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

fn gate_path(corpus: &str, gate: &str) -> String {
    format!("sec://{corpus}/{gate}")
}

pub async fn gates(pool: &PgPool, corpus_slug: Option<&str>) -> Result<SecurityGatesReport> {
    let rows = sqlx::query(
        r#"SELECT g.title AS gate,
                  g.project AS corpus,
                  COUNT(DISTINCT e.src_id) AS protected_functions
             FROM brain_vault_nodes g
             LEFT JOIN brain_vault_edges e
                    ON e.dst_id = g.id
                   AND e.edge_type = 'guarded_by'
                   AND COALESCE(e.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = g.project), 0)
                   )
            WHERE g.node_type = 'security:gate'
              AND ($1::text IS NULL OR g.project = $1)
              AND COALESCE(g.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = g.project), 0)
              )
            GROUP BY g.title, g.project
            ORDER BY g.project, g.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let gates = rows
        .into_iter()
        .map(|r| SecurityGateSummary {
            gate: r.get("gate"),
            corpus: r.get("corpus"),
            protected_functions: r.get("protected_functions"),
        })
        .collect();

    Ok(SecurityGatesReport {
        gates,
        unguarded_handlers: unguarded_handlers(pool, corpus_slug).await?,
    })
}

pub async fn guards(
    pool: &PgPool,
    corpus_slug: &str,
    symbol: &str,
) -> Result<Vec<SecurityGuardDetail>> {
    let symbols = resolve_symbol(pool, corpus_slug, symbol).await?;
    if symbols.is_empty() {
        return Err(anyhow!(
            "no symbol matching '{symbol}' in corpus '{corpus_slug}'"
        ));
    }

    let mut out = Vec::with_capacity(symbols.len());
    for sym in symbols {
        let rows = sqlx::query(
            r#"SELECT g.title AS gate, e.confidence, e.method
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes g ON g.id = e.dst_id
                WHERE e.src_id = $1
                  AND e.edge_type = 'guarded_by'
                  AND g.node_type = 'security:gate'
                  AND COALESCE(e.generation, 0) IN (
                      0,
                      COALESCE((SELECT current_generation
                                  FROM cortex_generations
                                 WHERE project = $2), 0)
                  )
                  AND COALESCE(g.generation, 0) IN (
                      0,
                      COALESCE((SELECT current_generation
                                  FROM cortex_generations
                                 WHERE project = $2), 0)
                  )
                ORDER BY g.title"#,
        )
        .bind(sym.id)
        .bind(corpus_slug)
        .fetch_all(pool)
        .await?;

        out.push(SecurityGuardDetail {
            symbol: sym.qualified_name,
            corpus: corpus_slug.to_string(),
            gates: rows
                .into_iter()
                .map(|r| SecurityGateRef {
                    gate: r.get("gate"),
                    confidence: r.get("confidence"),
                    method: r.get("method"),
                })
                .collect(),
        });
    }

    Ok(out)
}

async fn unguarded_handlers(
    pool: &PgPool,
    corpus_slug: Option<&str>,
) -> Result<Vec<SecurityFunctionRef>> {
    let rows = sqlx::query(
        r#"SELECT f.title AS qualified_name,
                  f.path,
                  f.start_line,
                  f.end_line
             FROM brain_vault_nodes f
            WHERE f.node_type = 'code:function'
              AND ($1::text IS NULL OR f.project = $1)
              AND f.valid_until IS NULL
              AND COALESCE(f.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = f.project), 0)
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges e
                    JOIN brain_vault_nodes g ON g.id = e.dst_id
                   WHERE e.src_id = f.id
                     AND e.edge_type = 'guarded_by'
                     AND g.node_type = 'security:gate'
                     AND COALESCE(e.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = f.project), 0)
                     )
              )
              AND (
                  lower(f.title) LIKE '%handler%'
                  OR lower(f.title) LIKE '%webhook%'
                  OR lower(f.title) LIKE '%enroll%'
                  OR lower(f.title) LIKE '%delegate%'
                  OR lower(f.title) LIKE '%secret%'
                  OR lower(f.title) LIKE '%config%'
                  OR lower(f.path) LIKE '%/ff-gateway/src/server.rs%'
                  OR lower(f.path) LIKE '%/ff-gateway/src/webhook.rs%'
                  OR lower(f.path) LIKE '%/ff-gateway/src/onboard.rs%'
              )
            ORDER BY f.title
            LIMIT 100"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SecurityFunctionRef {
            qualified_name: r.get("qualified_name"),
            path: r.get("path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
            confidence: None,
            method: None,
        })
        .collect())
}

#[derive(Debug, Clone)]
struct ResolvedSymbol {
    id: Uuid,
    qualified_name: String,
}

async fn resolve_symbol(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
) -> Result<Vec<ResolvedSymbol>> {
    let exact_path = format!("code://{corpus_slug}/{sel}");
    let rows = sqlx::query(
        r#"SELECT id, title
            FROM brain_vault_nodes
           WHERE project = $1
             AND node_type = 'code:function'
             AND (path = $2 OR title = $3 OR title LIKE $4)
             AND COALESCE(generation, 0) IN (
                 0,
                 COALESCE((SELECT current_generation
                             FROM cortex_generations
                            WHERE project = brain_vault_nodes.project), 0)
             )
           ORDER BY title COLLATE "C""#,
    )
    .bind(corpus_slug)
    .bind(&exact_path)
    .bind(sel)
    .bind(format!("%::{sel}"))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ResolvedSymbol {
            id: r.get("id"),
            qualified_name: r.get("title"),
        })
        .collect())
}
