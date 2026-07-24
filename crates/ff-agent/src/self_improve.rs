//! Recursive, evidence-driven improvement pass for ForgeFleet subsystems.
//!
//! The elected leader runs at most one subsystem pass per day.  Passes are
//! recorded in `ff_interactions`, while proposal and veto signatures live in
//! `work_items.metadata`; this deliberately avoids a second scheduling table.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tokio::process::Command;
use uuid::Uuid;

const PROJECT_ID: &str = "forge-fleet";
const MAX_FILED_PER_PASS: usize = 2;
const COUNCIL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const RESEARCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const SUBSYSTEMS: &[&str] = &[
    "router",
    "dispatch",
    "merge-drain",
    "memory/brain",
    "cortex",
    "scheduler",
    "telegram",
    "secrets",
    "model-lifecycle",
    "error-miner",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Proposal {
    title: String,
    description: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    external_unknowns: bool,
    #[serde(default)]
    research_query: Option<String>,
}

#[derive(Debug)]
struct OpenItem {
    title: String,
    signature: Option<String>,
    vetoed: bool,
}

#[derive(Debug)]
pub struct PassOutcome {
    pub subsystem: String,
    pub proposed: usize,
    pub filed: Vec<(Uuid, String)>,
    pub skipped: Vec<(String, String)>,
    pub research_queued: bool,
}

/// Run today's pass if it has not already completed. The caller owns the
/// leader gate; the durable pass claim closes the small hand-off race.
pub async fn tick(pool: &PgPool, worker_name: &str) -> Result<Option<PassOutcome>> {
    let today = Utc::now().date_naive();
    let subsystem = subsystem_for_date(today);
    let Some(pass_id) = claim_pass(pool, today, subsystem, worker_name).await? else {
        return Ok(None);
    };

    let result = run_pass(pool, subsystem).await;
    match &result {
        Ok(outcome) => {
            finish_pass(
                pool,
                pass_id,
                "ok",
                &serde_json::to_string(&outcome_summary(outcome))?,
            )
            .await?;
        }
        Err(error) => {
            finish_pass(pool, pass_id, "error", &error.to_string()).await?;
        }
    }
    result.map(Some)
}

fn subsystem_for_date(date: NaiveDate) -> &'static str {
    SUBSYSTEMS[date.num_days_from_ce().unsigned_abs() as usize % SUBSYSTEMS.len()]
}

async fn claim_pass(
    pool: &PgPool,
    date: NaiveDate,
    subsystem: &str,
    worker_name: &str,
) -> Result<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ff-self-improve-pass'))")
        .execute(&mut *tx)
        .await?;

    let already_ran: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM ff_interactions
             WHERE purpose = 'self-improve'
               AND request_meta->>'pass_date' = $1
               AND request_meta->>'subsystem' = $2
               AND (outcome = 'ok'
                    OR (outcome = 'running' AND ts >= NOW() - INTERVAL '2 hours')))",
    )
    .bind(date.to_string())
    .bind(subsystem)
    .fetch_one(&mut *tx)
    .await?;
    if already_ran {
        tx.commit().await?;
        return Ok(None);
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ff_interactions
             (channel, request_text, request_meta, engine, outcome, worker_name, purpose)
         VALUES ('self-improve', $1, $2, 'ff-council', 'running', $3, 'self-improve')
         RETURNING id",
    )
    .bind(format!("self-improvement pass for {subsystem}"))
    .bind(serde_json::json!({
        "project_id": PROJECT_ID,
        "pass_date": date,
        "subsystem": subsystem,
    }))
    .bind(worker_name)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(id))
}

async fn finish_pass(pool: &PgPool, id: Uuid, outcome: &str, response: &str) -> Result<()> {
    sqlx::query(
        "UPDATE ff_interactions
            SET outcome = $2,
                response_text = $3,
                error_text = CASE WHEN $2 = 'error' THEN $3 ELSE NULL END
          WHERE id = $1",
    )
    .bind(id)
    .bind(outcome)
    .bind(response)
    .execute(pool)
    .await?;
    Ok(())
}

async fn run_pass(pool: &PgPool, subsystem: &str) -> Result<PassOutcome> {
    let evidence = gather_evidence(pool, subsystem).await?;
    let prompt = charter_prompt(subsystem, &evidence);
    let council_output = run_ff(
        &["council", "--members", "codex,kimi", &prompt],
        COUNCIL_TIMEOUT,
    )
    .await
    .context("self-improve council")?;
    let proposals = parse_proposals(&council_output)?;

    let research_query = proposals
        .iter()
        .find(|p| p.external_unknowns)
        .and_then(|p| p.research_query.as_deref())
        .map(str::trim)
        .filter(|q| !q.is_empty());
    let research_queued = if let Some(query) = research_query {
        match run_ff(
            &[
                "research",
                "--detach",
                "--parallel",
                "3",
                "--depth",
                "3",
                query,
            ],
            RESEARCH_TIMEOUT,
        )
        .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(%error, subsystem, "self-improve research queue failed");
                false
            }
        }
    } else {
        false
    };

    let existing = load_existing_items(pool).await?;
    let mut filed = Vec::new();
    let mut skipped = Vec::new();
    for proposal in &proposals {
        if filed.len() >= MAX_FILED_PER_PASS {
            skipped.push((proposal.title.clone(), "pass cap reached".into()));
            continue;
        }
        let signature = proposal_signature(subsystem, &proposal.title);
        if existing
            .iter()
            .any(|item| item.vetoed && item.signature.as_deref() == Some(&signature))
        {
            skipped.push((proposal.title.clone(), "operator-vetoed signature".into()));
            continue;
        }
        if existing.iter().any(|item| {
            item.signature.as_deref() == Some(&signature)
                || title_similarity(&item.title, &proposal.title) >= 0.65
        }) {
            skipped.push((proposal.title.clone(), "similar open work item".into()));
            continue;
        }
        let id = file_proposal(pool, subsystem, proposal, &signature).await?;
        filed.push((id, proposal.title.clone()));
    }

    let outcome = PassOutcome {
        subsystem: subsystem.to_string(),
        proposed: proposals.len(),
        filed,
        skipped,
        research_queued,
    };
    send_digest(pool, &outcome).await;
    Ok(outcome)
}

async fn gather_evidence(pool: &PgPool, subsystem: &str) -> Result<String> {
    let errors = optional_relation_rows(
        pool,
        "error_signatures",
        subsystem,
        &["last_seen_at", "seen_at", "created_at"],
    )
    .await?;
    let utilization = optional_relation_rows(
        pool,
        "v_model_utilization",
        subsystem,
        &["bucket", "recorded_at", "ts", "created_at"],
    )
    .await?;

    let has_project_id: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name = 'ff_interactions'
               AND column_name = 'project_id')",
    )
    .fetch_one(pool)
    .await?;
    let project_predicate = if has_project_id {
        "i.project_id = $2"
    } else {
        "(i.request_meta->>'project_id' = $2 OR EXISTS (
            SELECT 1 FROM work_items w
             WHERE w.id = i.work_item_id AND w.project_id = $2))"
    };
    let sql = format!(
        "SELECT COALESCE(i.outcome, 'unknown') AS outcome,
                COALESCE(i.engine, 'unknown') AS engine,
                COUNT(*)::bigint AS count
           FROM ff_interactions i
          WHERE i.ts >= NOW() - INTERVAL '7 days'
            AND {project_predicate}
            AND concat_ws(' ', i.request_text, i.response_text, i.error_text,
                          i.request_meta::text) ILIKE '%' || $1 || '%'
          GROUP BY i.outcome, i.engine
          ORDER BY count DESC
          LIMIT 30"
    );
    let interaction_rows = sqlx::query(&sql)
        .bind(subsystem)
        .bind(PROJECT_ID)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "outcome": row.get::<String, _>("outcome"),
                "engine": row.get::<String, _>("engine"),
                "count": row.get::<i64, _>("count"),
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "window": "last 7 days",
        "subsystem": subsystem,
        "error_signatures": errors,
        "model_utilization": utilization,
        "interaction_counts": interaction_rows,
    }))?)
}

async fn optional_relation_rows(
    pool: &PgPool,
    relation: &str,
    subsystem: &str,
    timestamp_fields: &[&str],
) -> Result<serde_json::Value> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(relation)
        .fetch_one(pool)
        .await?;
    if !exists {
        return Ok(serde_json::json!({"available": false, "rows": []}));
    }

    let timestamp_expr = timestamp_fields
        .iter()
        .map(|field| format!("NULLIF(to_jsonb(t)->>'{field}', '')::timestamptz"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT to_jsonb(t)::text AS row
           FROM {relation} t
          WHERE COALESCE({timestamp_expr}, NOW()) >= NOW() - INTERVAL '7 days'
            AND to_jsonb(t)::text ILIKE '%' || $1 || '%'
          LIMIT 50"
    );
    let rows = sqlx::query_scalar::<_, String>(&sql)
        .bind(subsystem)
        .fetch_all(pool)
        .await?
        .into_iter()
        .filter_map(|row| serde_json::from_str::<serde_json::Value>(&row).ok())
        .collect::<Vec<_>>();
    Ok(serde_json::json!({"available": true, "rows": rows}))
}

fn charter_prompt(subsystem: &str, evidence: &str) -> String {
    format!(
        "Given this evidence, propose the 3 highest-impact improvements to \
         {subsystem}; concrete, file-scoped, buildable. Do not propose work that \
         already appears in the evidence. Flag external unknowns only when current \
         external research is genuinely required. Your final response MUST contain \
         exactly one JSON array between SELF_IMPROVE_JSON_BEGIN and \
         SELF_IMPROVE_JSON_END. Each object must have title, description, files \
         (array), external_unknowns (boolean), and research_query (string or null).\n\
         Evidence:\n{evidence}"
    )
}

fn parse_proposals(output: &str) -> Result<Vec<Proposal>> {
    let start = output
        .rfind("SELF_IMPROVE_JSON_BEGIN")
        .ok_or_else(|| anyhow!("council omitted SELF_IMPROVE_JSON_BEGIN"))?
        + "SELF_IMPROVE_JSON_BEGIN".len();
    let end = output[start..]
        .find("SELF_IMPROVE_JSON_END")
        .map(|offset| start + offset)
        .ok_or_else(|| anyhow!("council omitted SELF_IMPROVE_JSON_END"))?;
    let json = output[start..end]
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let mut proposals: Vec<Proposal> =
        serde_json::from_str(json).context("parse council proposal JSON")?;
    proposals.retain(|p| !p.title.trim().is_empty() && !p.description.trim().is_empty());
    proposals.truncate(3);
    if proposals.is_empty() {
        bail!("council returned no buildable proposals");
    }
    Ok(proposals)
}

async fn load_existing_items(pool: &PgPool) -> Result<Vec<OpenItem>> {
    let rows = sqlx::query(
        "SELECT title,
                metadata->>'self_improve_signature' AS signature,
                status = 'cancelled'
                  AND created_by = 'self-improve'
                  AND metadata ? 'self_improve_signature' AS vetoed
           FROM work_items
          WHERE (status NOT IN ('done', 'merged', 'failed', 'cancelled')
                 OR (status = 'cancelled' AND created_by = 'self-improve'))",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| OpenItem {
            title: row.get("title"),
            signature: row.get("signature"),
            vetoed: row.get("vetoed"),
        })
        .collect())
}

async fn file_proposal(
    pool: &PgPool,
    subsystem: &str,
    proposal: &Proposal,
    signature: &str,
) -> Result<Uuid> {
    let description = format!(
        "{}\n\nSuggested files:\n{}",
        proposal.description,
        proposal
            .files
            .iter()
            .map(|file| format!("- {file}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    sqlx::query_scalar(
        "INSERT INTO work_items
             (project_id, kind, title, description, status, priority, created_by,
              risk_score, metadata)
         VALUES ($1, 'feature', $2, $3, 'idea', 'medium', 'self-improve', 55, $4)
         RETURNING id",
    )
    .bind(PROJECT_ID)
    .bind(&proposal.title)
    .bind(description)
    .bind(serde_json::json!({
        "self_improve_signature": signature,
        "self_improve_subsystem": subsystem,
        "files": proposal.files,
        "operator_vetoable": true,
    }))
    .fetch_one(pool)
    .await
    .context("file self-improve work item")
}

fn proposal_signature(subsystem: &str, title: &str) -> String {
    format!(
        "{}:{}",
        normalize_words(subsystem).join("-"),
        normalize_words(title).join("-")
    )
}

fn title_similarity(left: &str, right: &str) -> f32 {
    let left: HashSet<String> = normalize_words(left).into_iter().collect();
    let right: HashSet<String> = normalize_words(right).into_iter().collect();
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count() as f32;
    let union = left.union(&right).count() as f32;
    intersection / union
}

fn normalize_words(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

async fn run_ff(args: &[&str], timeout: Duration) -> Result<String> {
    let binary = ff_binary();
    let mut command = Command::new(&binary);
    command.args(args);
    if let Ok(cwd) = std::env::current_dir() {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow!("{} timed out", args.first().unwrap_or(&"ff")))?
        .with_context(|| format!("spawn {}", binary.display()))?;
    if !output.status.success() {
        bail!(
            "ff {} exited {}: {}",
            args.first().unwrap_or(&""),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ff_binary() -> PathBuf {
    std::env::var_os("FORGEFLEET_FF_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe().ok().and_then(|exe| {
                let sibling = exe.with_file_name("ff");
                sibling.is_file().then_some(sibling)
            })
        })
        .unwrap_or_else(|| Path::new("ff").to_path_buf())
}

async fn send_digest(pool: &PgPool, outcome: &PassOutcome) {
    let filed = outcome
        .filed
        .iter()
        .map(|(_, title)| format!("FILED: {title}"))
        .chain(
            outcome
                .skipped
                .iter()
                .map(|(title, why)| format!("SKIPPED ({why}): {title}")),
        )
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "Subsystem: {}\nProposed: {}\nFiled: {}\nResearch queued: {}\n{}",
        outcome.subsystem,
        outcome.proposed,
        outcome.filed.len(),
        outcome.research_queued,
        filed
    );
    if let Err(error) =
        crate::telegram::send_telegram_from_secrets(pool, "ForgeFleet self-improve", &body).await
    {
        tracing::warn!(%error, "self-improve Telegram digest failed");
    }
}

fn outcome_summary(outcome: &PassOutcome) -> serde_json::Value {
    serde_json::json!({
        "subsystem": outcome.subsystem,
        "proposed": outcome.proposed,
        "filed": outcome.filed,
        "skipped": outcome.skipped,
        "research_queued": outcome.research_queued,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_is_stable_and_covers_every_subsystem() {
        let start = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let seen = (0..SUBSYSTEMS.len() as i64)
            .map(|days| subsystem_for_date(start + chrono::Duration::days(days)))
            .collect::<HashSet<_>>();
        assert_eq!(seen.len(), SUBSYSTEMS.len());
    }

    #[test]
    fn parses_last_marked_council_payload() {
        let output = r#"noise SELF_IMPROVE_JSON_BEGIN
        [{"title":"Bound retries","description":"Add a retry budget","files":["router.rs"],
          "external_unknowns":false,"research_query":null}]
        SELF_IMPROVE_JSON_END"#;
        let proposals = parse_proposals(output).unwrap();
        assert_eq!(proposals[0].title, "Bound retries");
    }

    #[test]
    fn title_similarity_dedupes_reworded_ideas() {
        assert!(
            title_similarity(
                "Add bounded retries to router dispatch",
                "Router dispatch: add bounded retries"
            ) >= 0.65
        );
        assert!(title_similarity("Rotate secrets", "Index Cortex symbols") < 0.65);
    }

    #[test]
    fn signatures_are_stable_across_punctuation_and_case() {
        assert_eq!(
            proposal_signature("merge-drain", "Fix: Stuck PRs!"),
            proposal_signature("MERGE drain", "fix stuck PRs")
        );
    }
}
