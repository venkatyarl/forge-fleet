use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    pub severity: String,
    pub node_path: String,
    pub node_title: String,
    pub detail: String,
}

pub async fn audit(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    findings.extend(endpoints_no_guard(pool, corpus).await?);
    findings.extend(dead_column_readwrite(pool, corpus).await?);
    findings.extend(orphan_topic(pool, corpus).await?);
    findings.extend(unreferenced_migration(pool, corpus).await?);
    findings.extend(swallowed_error(pool, corpus).await?);
    findings.sort_by(|a, b| {
        severity_rank(&a.severity)
            .cmp(&severity_rank(&b.severity))
            .then_with(|| a.rule.cmp(&b.rule))
            .then_with(|| a.node_title.cmp(&b.node_title))
    });
    Ok(findings)
}

pub async fn handle_cli(
    pool: &PgPool,
    corpus: Option<&str>,
    emit_work_items: bool,
    format: &str,
) -> Result<()> {
    let findings = audit(pool, corpus).await?;
    print_findings(&findings, format)?;
    if emit_work_items {
        emit_findings_as_work_items(pool, corpus, &findings).await?;
    }
    Ok(())
}

async fn endpoints_no_guard(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<Finding>> {
    query_findings(
        pool,
        corpus,
        r#"SELECT 'endpoints-no-guard' AS rule,
                  'high' AS severity,
                  ep.path AS node_path,
                  ep.title AS node_title,
                  'candidate unauthenticated route: handler '
                      || COALESCE(h.title, '<unresolved>')
                      || ' has no guarded_by security:gate edge' AS detail
             FROM brain_vault_nodes ep
             JOIN brain_vault_edges se
               ON se.src_id = ep.id
              AND se.edge_type = 'serves'
              AND COALESCE(se.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = ep.project), 0)
              )
             JOIN brain_vault_nodes h
               ON h.id = se.dst_id
              AND h.node_type = 'code:function'
              AND h.valid_until IS NULL
              AND COALESCE(h.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = ep.project), 0)
              )
            WHERE ep.node_type = 'http:endpoint'
              AND ep.valid_until IS NULL
              AND ($1::text IS NULL OR ep.project = $1)
              AND COALESCE(ep.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = ep.project), 0)
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges ge
                    JOIN brain_vault_nodes g ON g.id = ge.dst_id
                   WHERE ge.src_id = h.id
                     AND ge.edge_type = 'guarded_by'
                     AND g.node_type = 'security:gate'
                     AND COALESCE(ge.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = ep.project), 0)
                     )
                     AND COALESCE(g.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = ep.project), 0)
                     )
              )"#,
    )
    .await
}

async fn dead_column_readwrite(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<Finding>> {
    query_findings(
        pool,
        corpus,
        r#"SELECT 'dead-column-readwrite' AS rule,
                  'medium' AS severity,
                  c.path AS node_path,
                  c.title AS node_title,
                  'dead schema candidate: column has no incoming reads or writes edges' AS detail
             FROM brain_vault_nodes c
            WHERE c.node_type = 'db:column'
              AND ($1::text IS NULL OR c.project = $1)
              AND COALESCE(c.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges e
                   WHERE e.dst_id = c.id
                     AND e.edge_type IN ('reads', 'writes')
                     AND COALESCE(e.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = c.project), 0)
                     )
              )"#,
    )
    .await
}

async fn orphan_topic(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<Finding>> {
    query_findings(
        pool,
        corpus,
        r#"SELECT 'orphan-topic' AS rule,
                  'medium' AS severity,
                  t.path AS node_path,
                  t.title AS node_title,
                  'event topic is published but has no direct subscriber and no matching wildcard subscriber' AS detail
             FROM brain_vault_nodes t
            WHERE t.node_type = 'event:topic'
              AND ($1::text IS NULL OR t.project = $1)
              AND COALESCE(t.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = t.project), 0)
              )
              AND EXISTS (
                  SELECT 1
                    FROM brain_vault_edges p
                   WHERE p.dst_id = t.id
                     AND p.edge_type = 'publishes'
                     AND COALESCE(p.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = t.project), 0)
                     )
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges s
                   WHERE s.dst_id = t.id
                     AND s.edge_type = 'subscribes'
                     AND COALESCE(s.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = t.project), 0)
                     )
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_nodes wt
                    JOIN brain_vault_edges ws
                      ON ws.dst_id = wt.id
                     AND ws.edge_type = 'subscribes'
                   WHERE wt.node_type = 'event:topic'
                     AND wt.project = t.project
                     AND (wt.title = '>' OR (wt.title LIKE '%.>' AND t.title LIKE (LEFT(wt.title, LENGTH(wt.title) - 1) || '%')))
                     AND COALESCE(wt.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = t.project), 0)
                     )
                     AND COALESCE(ws.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = t.project), 0)
                     )
              )"#,
    )
    .await
}

async fn unreferenced_migration(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<Finding>> {
    query_findings(
        pool,
        corpus,
        r#"SELECT DISTINCT 'unreferenced-migration' AS rule,
                  'low' AS severity,
                  m.path AS node_path,
                  m.title AS node_title,
                  'migration creates table '
                      || t.title
                      || ' with no incoming reads/writes and no code reference' AS detail
             FROM brain_vault_nodes m
             JOIN brain_vault_edges ce
               ON ce.src_id = m.id
              AND ce.edge_type = 'creates'
              AND COALESCE(ce.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = m.project), 0)
              )
             JOIN brain_vault_nodes t
               ON t.id = ce.dst_id
              AND t.node_type = 'db:table'
              AND COALESCE(t.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = m.project), 0)
              )
            WHERE m.node_type = 'db:migration'
              AND ($1::text IS NULL OR m.project = $1)
              AND COALESCE(m.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = m.project), 0)
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges rw
                   WHERE rw.dst_id = t.id
                     AND rw.edge_type IN ('reads', 'writes')
                     AND COALESCE(rw.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = m.project), 0)
                     )
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges ref
                    JOIN brain_vault_nodes code
                      ON (code.id = ref.src_id OR code.id = ref.dst_id)
                   WHERE (ref.src_id = t.id OR ref.dst_id = t.id)
                     AND code.node_type LIKE 'code:%'
                     AND COALESCE(ref.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = m.project), 0)
                     )
                     AND COALESCE(code.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = m.project), 0)
                     )
              )"#,
    )
    .await
}

async fn swallowed_error(pool: &PgPool, corpus: Option<&str>) -> Result<Vec<Finding>> {
    query_findings(
        pool,
        corpus,
        r#"SELECT 'swallowed-error' AS rule,
                  'low' AS severity,
                  err.path AS node_path,
                  err.title AS node_title,
                  'error type is defined but has no code:function returns edge' AS detail
             FROM brain_vault_nodes err
            WHERE err.node_type = 'error:type'
              AND ($1::text IS NULL OR err.project = $1)
              AND COALESCE(err.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = err.project), 0)
              )
              AND EXISTS (
                  SELECT 1
                    FROM brain_vault_edges de
                   WHERE de.dst_id = err.id
                     AND de.edge_type = 'defines_error'
                     AND COALESCE(de.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = err.project), 0)
                     )
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM brain_vault_edges ret
                    JOIN brain_vault_nodes f ON f.id = ret.src_id
                   WHERE ret.dst_id = err.id
                     AND ret.edge_type = 'returns'
                     AND f.node_type = 'code:function'
                     AND COALESCE(ret.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = err.project), 0)
                     )
                     AND COALESCE(f.generation, 0) IN (
                         0,
                         COALESCE((SELECT current_generation
                                     FROM cortex_generations
                                    WHERE project = err.project), 0)
                     )
              )"#,
    )
    .await
}

async fn query_findings(pool: &PgPool, corpus: Option<&str>, sql: &str) -> Result<Vec<Finding>> {
    let rows = sqlx::query(sql).bind(corpus).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| Finding {
            rule: row.get("rule"),
            severity: row.get("severity"),
            node_path: row.get("node_path"),
            node_title: row.get("node_title"),
            detail: row.get("detail"),
        })
        .collect())
}

fn print_findings(findings: &[Finding], format: &str) -> Result<()> {
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(findings)?);
        return Ok(());
    }

    let counts = counts_by_rule(findings);
    if findings.is_empty() {
        println!("cortex audit: no findings");
        return Ok(());
    }

    println!("cortex audit: {} finding(s)", findings.len());
    for (rule, count) in counts {
        println!("  {rule}: {count}");
    }
    println!();
    println!("{:<24} {:<7} {:<48} DETAIL", "RULE", "SEV", "NODE");
    for finding in findings {
        println!(
            "{:<24} {:<7} {:<48} {}",
            truncate(&finding.rule, 24),
            finding.severity,
            truncate(&finding.node_title, 48),
            finding.detail
        );
    }
    Ok(())
}

async fn emit_findings_as_work_items(
    pool: &PgPool,
    corpus: Option<&str>,
    findings: &[Finding],
) -> Result<()> {
    if findings.is_empty() {
        println!("would emit 0 work items");
        return Ok(());
    }

    let Some(project_id) = resolve_emit_project(pool, corpus, findings).await? else {
        println!("would emit {} work items", findings.len());
        return Ok(());
    };

    let mut inserted = 0usize;
    for finding in findings {
        let title = format!("[cortex audit] {}: {}", finding.rule, finding.node_title);
        sqlx::query(
            r#"INSERT INTO work_items
                   (project_id, kind, title, description, priority, created_by, metadata)
               VALUES ($1, 'audit', $2, $3, $4, 'ff cortex audit',
                       jsonb_build_object(
                           'node_path', $5::text,
                           'rule', $6::text,
                           'severity', $7::text,
                           'source', 'cortex audit'
                       ))"#,
        )
        .bind(&project_id)
        .bind(title)
        .bind(&finding.detail)
        .bind(priority_for_severity(&finding.severity))
        .bind(&finding.node_path)
        .bind(&finding.rule)
        .bind(&finding.severity)
        .execute(pool)
        .await?;
        inserted += 1;
    }

    println!("emitted {inserted} work item(s) into project {project_id}");
    Ok(())
}

async fn resolve_emit_project(
    pool: &PgPool,
    corpus: Option<&str>,
    findings: &[Finding],
) -> Result<Option<String>> {
    if let Some(corpus) = corpus {
        return project_exists(pool, corpus)
            .await
            .map(|exists| exists.then(|| corpus.to_string()));
    }

    let mut corpus_counts: BTreeMap<String, usize> = BTreeMap::new();
    for finding in findings {
        if let Some(corpus) = corpus_from_node_path(&finding.node_path) {
            *corpus_counts.entry(corpus.to_string()).or_default() += 1;
        }
    }
    if corpus_counts.len() != 1 {
        return Ok(None);
    }
    let project_id = corpus_counts.into_keys().next().unwrap_or_default();
    project_exists(pool, &project_id)
        .await
        .map(|exists| exists.then_some(project_id))
}

async fn project_exists(pool: &PgPool, project_id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1)")
            .bind(project_id)
            .fetch_one(pool)
            .await?,
    )
}

fn counts_by_rule(findings: &[Finding]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for finding in findings {
        *counts.entry(finding.rule.clone()).or_default() += 1;
    }
    counts
}

fn corpus_from_node_path(path: &str) -> Option<&str> {
    let (_, rest) = path.split_once("://")?;
    rest.split('/').next().filter(|s| !s.is_empty())
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "high" => 0,
        "medium" => 1,
        "low" => 2,
        _ => 3,
    }
}

fn priority_for_severity(severity: &str) -> &'static str {
    match severity {
        "high" => "high",
        "medium" => "normal",
        _ => "low",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}
