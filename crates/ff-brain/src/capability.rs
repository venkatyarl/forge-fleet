//! Shared capability broker used by the MCP and CLI query surfaces.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityKind {
    Skill,
    Tool,
    Task,
}

impl CapabilityKind {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "skill" => Ok(Self::Skill),
            "tool" => Ok(Self::Tool),
            "task" => Ok(Self::Task),
            _ => Err("kind must be one of: skill, tool, task".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EstCostClass {
    LocalFree,
    CloudPaid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityMatch {
    pub name: String,
    pub kind: String,
    pub is_local: bool,
    pub invoke_hint: String,
    pub est_cost_class: EstCostClass,
    #[serde(skip)]
    score: f64,
    #[serde(skip)]
    available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityResult {
    pub have: bool,
    pub matches: Vec<CapabilityMatch>,
    pub best: Option<CapabilityMatch>,
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut aa, mut bb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        aa += f64::from(*x) * f64::from(*x);
        bb += f64::from(*y) * f64::from(*y);
    }
    if aa == 0.0 || bb == 0.0 {
        0.0
    } else {
        dot / (aa.sqrt() * bb.sqrt())
    }
}

fn finish(mut matches: Vec<CapabilityMatch>) -> CapabilityResult {
    matches.sort_by(|a, b| {
        b.available
            .cmp(&a.available)
            .then_with(|| b.is_local.cmp(&a.is_local))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.name.cmp(&b.name))
    });
    matches.truncate(5);
    CapabilityResult {
        have: matches.iter().any(|candidate| candidate.available),
        best: matches.first().cloned(),
        matches,
    }
}

async fn skill_matches(pool: &PgPool, need: &str) -> Result<Vec<CapabilityMatch>, String> {
    let rows = sqlx::query(
        "SELECT name, description, when_to_invoke
           FROM skills
          WHERE superseded_by IS NULL
          ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("skills query: {e}"))?;

    let candidates: Vec<(String, String)> = rows
        .into_iter()
        .map(|row| {
            let name: String = row.get("name");
            let description: String = row
                .get::<Option<String>, _>("description")
                .unwrap_or_default();
            let invoke: String = row
                .get::<Option<String>, _>("when_to_invoke")
                .unwrap_or_default();
            (name, format!("{description}\n{invoke}"))
        })
        .collect();

    let semantic = if let Some(client) = crate::embeddings::fleet_embedding_client(pool).await {
        let documents: Vec<String> = candidates
            .iter()
            .map(|(name, description)| format!("{name}\n{description}"))
            .collect();
        let mut texts = Vec::with_capacity(documents.len() + 1);
        texts.push(need);
        texts.extend(documents.iter().map(String::as_str));
        client.embed_batch(&texts).await.ok()
    } else {
        None
    };

    let mut out = Vec::new();
    for (index, (name, description)) in candidates.into_iter().enumerate() {
        let score = semantic
            .as_ref()
            .and_then(|vectors| {
                vectors.first().and_then(|query| {
                    vectors
                        .get(index + 1)
                        .map(|candidate| cosine_similarity(query, candidate))
                })
            })
            .unwrap_or_else(|| ff_skills::selector::lexical_relevance(need, &name, &description));
        if score >= if semantic.is_some() { 0.35 } else { 0.08 } {
            out.push(CapabilityMatch {
                invoke_hint: format!("ff skills show {name}"),
                name,
                kind: "skill".into(),
                is_local: true,
                est_cost_class: EstCostClass::LocalFree,
                score,
                available: true,
            });
        }
    }
    Ok(out)
}

async fn tool_matches(pool: &PgPool, need: &str) -> Result<Vec<CapabilityMatch>, String> {
    let this_worker = ff_agent::fleet_info::resolve_this_worker_name().await;
    let rows = sqlx::query(
        "SELECT et.id, et.display_name, et.cli_entrypoint, et.mcp_server_command,
                EXISTS (
                    SELECT 1 FROM computer_external_tools cet
                    WHERE cet.tool_id = et.id AND cet.status = 'ok'
                ) AS installed_any,
                EXISTS (
                    SELECT 1
                      FROM computer_external_tools cet
                      JOIN computers c ON c.id = cet.computer_id
                     WHERE cet.tool_id = et.id
                       AND cet.status = 'ok'
                       AND LOWER(c.name) = LOWER($1)
                ) AS installed_local
           FROM external_tools et
          ORDER BY et.id",
    )
    .bind(&this_worker)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("external_tools query: {e}"))?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id: String = row.get("id");
            let display: String = row.get("display_name");
            let score = ff_skills::selector::lexical_relevance(need, &id, &display);
            (score >= 0.08).then(|| {
                let cli: Option<String> = row.get("cli_entrypoint");
                let mcp: Option<String> = row.get("mcp_server_command");
                let installed_any: bool = row.get("installed_any");
                let installed_local: bool = row.get("installed_local");
                CapabilityMatch {
                    name: id.clone(),
                    kind: "tool".into(),
                    is_local: installed_local,
                    invoke_hint: if installed_any {
                        cli.or(mcp).unwrap_or_else(|| id.clone())
                    } else {
                        format!("ff ext install {id} --yes")
                    },
                    est_cost_class: EstCostClass::LocalFree,
                    score,
                    available: installed_any,
                }
            })
        })
        .collect())
}

async fn task_matches(pool: &PgPool, need: &str) -> Result<Vec<CapabilityMatch>, String> {
    let this_worker = ff_agent::fleet_info::resolve_this_worker_name().await;
    let rows = ff_db::pg_route_deployments(
        pool,
        &ff_db::RouteFilter {
            workload: Some(need.to_string()),
            max_health_age_sec: Some(ff_db::DISPATCH_HEALTH_MAX_AGE_SEC),
            limit: 5,
            ..Default::default()
        },
    )
    .await
    .map_err(|e| format!("fleet route: {e}"))?;
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let name = row
                .catalog_name
                .or(row.catalog_id)
                .unwrap_or_else(|| format!("{}:{}", row.worker_name, row.port));
            CapabilityMatch {
                invoke_hint: format!("ff --model {name} run {need:?}"),
                name,
                kind: "task".into(),
                is_local: row.worker_name.eq_ignore_ascii_case(&this_worker),
                est_cost_class: EstCostClass::LocalFree,
                score: 1.0 / (index + 1) as f64,
                available: true,
            }
        })
        .collect())
}

pub async fn capability_check(
    pool: &PgPool,
    need: &str,
    kind: CapabilityKind,
) -> Result<CapabilityResult, String> {
    let need = need.trim();
    if need.is_empty() {
        return Err("need must not be empty".into());
    }
    let matches = match kind {
        CapabilityKind::Skill => skill_matches(pool, need).await?,
        CapabilityKind::Tool => tool_matches(pool, need).await?,
        CapabilityKind::Task => task_matches(pool, need).await?,
    };
    Ok(finish(matches))
}

/// Resolve a need across every capability source. Used by the zero-option CLI
/// form (`ff capability <need>`); MCP callers pass an explicit kind.
pub async fn capability_check_all(pool: &PgPool, need: &str) -> Result<CapabilityResult, String> {
    let mut matches = skill_matches(pool, need).await?;
    matches.extend(tool_matches(pool, need).await?);
    matches.extend(task_matches(pool, need).await?);
    Ok(finish(matches))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_prefers_local_before_higher_score_cloud() {
        let result = finish(vec![
            CapabilityMatch {
                name: "cloud".into(),
                kind: "tool".into(),
                is_local: false,
                invoke_hint: "cloud".into(),
                est_cost_class: EstCostClass::CloudPaid,
                score: 1.0,
                available: true,
            },
            CapabilityMatch {
                name: "local".into(),
                kind: "tool".into(),
                is_local: true,
                invoke_hint: "local".into(),
                est_cost_class: EstCostClass::LocalFree,
                score: 0.2,
                available: true,
            },
        ]);
        assert_eq!(result.best.unwrap().name, "local");
    }

    #[test]
    fn empty_result_does_not_claim_capability() {
        let result = finish(Vec::new());
        assert!(!result.have);
        assert!(result.best.is_none());
    }
}
