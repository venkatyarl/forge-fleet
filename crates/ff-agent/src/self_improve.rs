//! Evidence-driven, operator-vetoable self-improvement proposals.
//!
//! The daemon runs this tick on every node, but the process-local leader cache
//! gates every pass. One subsystem is selected per UTC day; the interaction
//! log is the durable pass cursor, so leader handoff cannot repeat a pass.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

const PROJECT_ID: &str = "forge-fleet";
const PASS_PURPOSE: &str = "self-improve-pass";
const PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_FILED: usize = 2;
const SUBSYSTEMS: [&str; 10] = [
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Proposal {
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CouncilResponse {
    #[serde(default)]
    proposals: Vec<Proposal>,
    #[serde(default)]
    research_needed: bool,
    #[serde(default)]
    research_query: Option<String>,
}

#[derive(Debug)]
struct ExistingItem {
    title: String,
    signature: Option<String>,
    vetoed: bool,
    open: bool,
}

/// Leader-only periodic task. A daily rotation across ten subsystems means
/// every subsystem is revisited every ten days (and therefore never more often
/// than weekly).
pub struct SelfImproveTick {
    pg: PgPool,
}

impl SelfImproveTick {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PASS_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if crate::leader_cache::is_current_leader()
                            && let Err(error) = self.run_once().await
                        {
                            warn!(%error, "self-improve pass failed");
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        })
    }

    pub async fn run_once(&self) -> Result<usize> {
        let now = Utc::now();
        let subsystem = subsystem_for_day(now.num_days_from_ce() as i64);
        if pass_already_recorded(&self.pg, subsystem).await? {
            return Ok(0);
        }

        // The advisory lock closes the small race between two daemons whose
        // process-local leader caches overlap during handoff.
        let mut lock_connection = self.pg.acquire().await?;
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext('ff-self-improve'))")
                .fetch_one(&mut *lock_connection)
                .await?;
        if !locked {
            return Ok(0);
        }
        let result = self.run_locked(subsystem).await;
        let _: Result<bool, _> =
            sqlx::query_scalar("SELECT pg_advisory_unlock(hashtext('ff-self-improve'))")
                .fetch_one(&mut *lock_connection)
                .await;
        result
    }

    async fn run_locked(&self, subsystem: &str) -> Result<usize> {
        if pass_already_recorded(&self.pg, subsystem).await? {
            return Ok(0);
        }

        let evidence = gather_evidence(&self.pg, subsystem).await?;
        let charter = format!(
            "Given this last-7d ForgeFleet evidence, propose the 3 highest-impact improvements \
             to {subsystem}; concrete, file-scoped, buildable. Return ONLY JSON shaped \
             {{\"proposals\":[{{\"title\":\"...\",\"description\":\"...\",\"files\":[\"...\"]}}],\
             \"research_needed\":false,\"research_query\":null}}. Flag research only for an \
             external unknown that materially blocks a proposal.\n\nEvidence:\n{evidence}"
        );
        let council_output = run_ff(&["council", "--members", "codex,kimi", &charter]).await?;
        let response = parse_council_response(&council_output)?;
        validate_council_response(&response)?;

        let research = if response.research_needed {
            if let Some(query) = response
                .research_query
                .as_deref()
                .filter(|q| !q.trim().is_empty())
            {
                Some(run_ff(&["research", query]).await.unwrap_or_else(|error| {
                    warn!(%error, "self-improve optional research failed");
                    format!("research unavailable: {error}")
                }))
            } else {
                None
            }
        } else {
            None
        };

        let existing = load_existing_items(&self.pg).await?;
        let mut filed = Vec::new();
        for proposal in response.proposals.iter().take(3) {
            let signature = proposal_signature(subsystem, &proposal.title);
            if should_skip(proposal, &signature, &existing, &filed) {
                continue;
            }
            file_proposal(
                &self.pg,
                subsystem,
                proposal,
                &signature,
                research.as_deref(),
            )
            .await?;
            filed.push(proposal.title.clone());
            if filed.len() == MAX_FILED {
                break;
            }
        }

        record_pass(&self.pg, subsystem, response.proposals.len(), filed.len()).await?;
        let digest = render_digest(subsystem, &response.proposals, &filed);
        if let Err(error) =
            crate::telegram::send_telegram_from_secrets(&self.pg, "Self-improve council", &digest)
                .await
        {
            warn!(%error, "self-improve Telegram digest failed");
        }
        info!(
            subsystem,
            proposed = response.proposals.len(),
            filed = filed.len(),
            "self-improve pass complete"
        );
        Ok(filed.len())
    }
}

fn subsystem_for_day(days_from_ce: i64) -> &'static str {
    SUBSYSTEMS[days_from_ce.rem_euclid(SUBSYSTEMS.len() as i64) as usize]
}

async fn pass_already_recorded(pg: &PgPool, subsystem: &str) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM ff_interactions
              WHERE purpose = $1
                AND request_meta->>'subsystem' = $2
                AND ts >= NOW() - INTERVAL '7 days'
         )",
    )
    .bind(PASS_PURPOSE)
    .bind(subsystem)
    .fetch_one(pg)
    .await
    .context("check self-improve pass cursor")
}

async fn gather_evidence(pg: &PgPool, subsystem: &str) -> Result<String> {
    let interactions: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object(
             'total', COUNT(*),
             'errors', COUNT(*) FILTER (WHERE outcome = 'error' OR error_text IS NOT NULL),
             'purposes', COALESCE(jsonb_object_agg(purpose, count), '{}'::jsonb)
         )
         FROM (
             SELECT COALESCE(purpose, 'unknown') AS purpose, outcome, error_text,
                    COUNT(*) OVER (PARTITION BY COALESCE(purpose, 'unknown')) AS count
               FROM ff_interactions
              WHERE ts >= NOW() - INTERVAL '7 days'
                AND (request_text ILIKE '%' || $1 || '%'
                     OR COALESCE(purpose, '') ILIKE '%' || $1 || '%'
                     OR COALESCE(request_meta::text, '') ILIKE '%' || $1 || '%')
         ) recent",
    )
    .bind(subsystem)
    .fetch_one(pg)
    .await
    .unwrap_or_else(|_| json!({"unavailable": true}));

    let mut error_signatures = optional_relation_slice(pg, "error_signatures", subsystem).await;
    if error_signatures.get("unavailable").is_some() {
        error_signatures = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(row_data), '[]'::jsonb) FROM (
                 SELECT jsonb_build_object(
                            'signature', COALESCE(error_signature, error_text),
                            'occurrences', COUNT(*),
                            'last_seen', MAX(ts)
                        ) AS row_data
                   FROM ff_interactions
                  WHERE ts >= NOW() - INTERVAL '7 days'
                    AND (error_signature IS NOT NULL OR error_text IS NOT NULL)
                    AND (request_text ILIKE '%' || $1 || '%'
                         OR COALESCE(purpose, '') ILIKE '%' || $1 || '%'
                         OR COALESCE(request_meta::text, '') ILIKE '%' || $1 || '%')
                  GROUP BY COALESCE(error_signature, error_text)
                  ORDER BY COUNT(*) DESC
                  LIMIT 50
             ) fallback",
        )
        .bind(subsystem)
        .fetch_one(pg)
        .await
        .unwrap_or_else(|error| json!({"unavailable": true, "reason": error.to_string()}));
    }
    let model_utilization = optional_relation_slice(pg, "v_model_utilization", subsystem).await;
    Ok(serde_json::to_string_pretty(&json!({
        "subsystem": subsystem,
        "window": "7 days",
        "error_signatures": error_signatures,
        "model_utilization": model_utilization,
        "interactions": interactions,
    }))?)
}

async fn optional_relation_slice(pg: &PgPool, relation: &str, subsystem: &str) -> Value {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(relation)
        .fetch_one(pg)
        .await
        .unwrap_or(false);
    if !exists {
        return json!({"unavailable": true, "reason": format!("{relation} not installed")});
    }
    let timestamp_column: Option<String> = sqlx::query_scalar(
        "SELECT column_name
           FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = $1
            AND column_name = ANY($2)
          ORDER BY array_position($2, column_name)
          LIMIT 1",
    )
    .bind(relation)
    .bind(vec![
        "ts",
        "recorded_at",
        "created_at",
        "window_start",
        "captured_at",
    ])
    .fetch_optional(pg)
    .await
    .ok()
    .flatten();
    let time_predicate = timestamp_column
        .map(|column| format!(" AND r.\"{column}\" >= NOW() - INTERVAL '7 days'"))
        .unwrap_or_default();
    // `relation` and `timestamp_column` are selected exclusively from constants
    // and information_schema respectively.
    let sql = format!(
        "SELECT COALESCE(jsonb_agg(row_data), '[]'::jsonb) FROM (
             SELECT to_jsonb(r) AS row_data FROM {relation} r
              WHERE to_jsonb(r)::text ILIKE '%' || $1 || '%'
              {time_predicate}
              LIMIT 50
         ) rows"
    );
    sqlx::query_scalar(&sql)
        .bind(subsystem)
        .fetch_one(pg)
        .await
        .unwrap_or_else(|error| json!({"unavailable": true, "reason": error.to_string()}))
}

async fn run_ff(args: &[&str]) -> Result<String> {
    let mut command = Command::new("ff");
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .context("ff command timed out")?
        .context("start ff command")?;
    if !output.status.success() {
        return Err(anyhow!(
            "ff command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_council_response(output: &str) -> Result<CouncilResponse> {
    if let Ok(parsed) = serde_json::from_str(output.trim()) {
        return Ok(parsed);
    }
    let start = output
        .find('{')
        .context("council response contained no JSON")?;
    let end = output
        .rfind('}')
        .context("council response contained no JSON")?;
    serde_json::from_str(&output[start..=end]).context("parse council response JSON")
}

fn validate_council_response(response: &CouncilResponse) -> Result<()> {
    if response.proposals.len() != 3 {
        return Err(anyhow!(
            "council must return exactly 3 proposals, got {}",
            response.proposals.len()
        ));
    }
    for proposal in &response.proposals {
        if proposal.title.trim().is_empty()
            || proposal.description.trim().is_empty()
            || proposal.files.is_empty()
        {
            return Err(anyhow!(
                "council proposal must have a title, description, and files"
            ));
        }
        if proposal.files.iter().any(|file| {
            file.trim().is_empty()
                || file.starts_with('/')
                || file.split('/').any(|part| part == "..")
        }) {
            return Err(anyhow!("council proposal contains an unsafe file path"));
        }
    }
    Ok(())
}

async fn load_existing_items(pg: &PgPool) -> Result<Vec<ExistingItem>> {
    let rows = sqlx::query(
        "SELECT title, status,
                metadata->>'self_improve_signature' AS signature,
                COALESCE((metadata->>'operator_vetoed')::boolean, false) AS operator_vetoed
           FROM work_items
          WHERE created_by = 'self-improve'
             OR status NOT IN ('completed', 'failed', 'cancelled')",
    )
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let status: String = row.get("status");
            ExistingItem {
                title: row.get("title"),
                signature: row.try_get("signature").ok().flatten(),
                vetoed: row.get::<bool, _>("operator_vetoed")
                    || matches!(status.as_str(), "cancelled" | "vetoed"),
                open: !matches!(
                    status.as_str(),
                    "completed" | "failed" | "cancelled" | "vetoed"
                ),
            }
        })
        .collect())
}

fn should_skip(
    proposal: &Proposal,
    signature: &str,
    existing: &[ExistingItem],
    newly_filed: &[String],
) -> bool {
    existing.iter().any(|item| {
        item.signature.as_deref() == Some(signature) && item.vetoed
            || item.open && title_similarity(&item.title, &proposal.title) >= 0.6
    }) || newly_filed
        .iter()
        .any(|title| title_similarity(title, &proposal.title) >= 0.6)
}

fn proposal_signature(subsystem: &str, title: &str) -> String {
    let normalized = title_words(title).into_iter().collect::<Vec<_>>().join(" ");
    format!("{:x}", Sha256::digest(format!("{subsystem}:{normalized}")))
}

fn title_words(title: &str) -> HashSet<String> {
    title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(|word| {
            let mut word = word.to_ascii_lowercase();
            if word.len() > 4 && word.ends_with('s') {
                word.pop();
            }
            word
        })
        .collect()
}

fn title_similarity(left: &str, right: &str) -> f32 {
    let left = title_words(left);
    let right = title_words(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count() as f32;
    intersection / left.union(&right).count() as f32
}

async fn file_proposal(
    pg: &PgPool,
    subsystem: &str,
    proposal: &Proposal,
    signature: &str,
    research: Option<&str>,
) -> Result<()> {
    let description = if proposal.files.is_empty() {
        proposal.description.clone()
    } else {
        format!(
            "{}\n\nFiles: {}",
            proposal.description,
            proposal.files.join(", ")
        )
    };
    sqlx::query(
        "INSERT INTO work_items
             (project_id, kind, title, description, status, priority, created_by,
              risk_score, metadata)
         VALUES ($1, 'feature', $2, $3, 'ready', 'medium', 'self-improve', 55,
                 jsonb_build_object(
                     'self_improve_signature', $4::text,
                     'self_improve_subsystem', $5::text,
                     'operator_vetoed', false,
                     'research', $6::text
                 ))",
    )
    .bind(PROJECT_ID)
    .bind(proposal.title.trim())
    .bind(description)
    .bind(signature)
    .bind(subsystem)
    .bind(research)
    .execute(pg)
    .await
    .context("file self-improve work item")?;
    Ok(())
}

async fn record_pass(pg: &PgPool, subsystem: &str, proposed: usize, filed: usize) -> Result<()> {
    sqlx::query(
        "INSERT INTO ff_interactions
             (id, ts, channel, request_text, request_meta, engine, response_text,
              outcome, purpose)
         VALUES ($1, NOW(), 'daemon', $2, $3, 'ff-council',
                 $4, 'success', $5)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(format!("self-improve council pass for {subsystem}"))
    .bind(json!({"project_id": PROJECT_ID, "subsystem": subsystem}))
    .bind(format!("proposed={proposed} filed={filed}"))
    .bind(PASS_PURPOSE)
    .execute(pg)
    .await
    .context("record self-improve pass")?;
    Ok(())
}

fn render_digest(subsystem: &str, proposed: &[Proposal], filed: &[String]) -> String {
    let proposed = proposed
        .iter()
        .map(|proposal| format!("• {}", proposal.title))
        .collect::<Vec<_>>()
        .join("\n");
    let filed = if filed.is_empty() {
        "• none (deduplicated or vetoed)".to_string()
    } else {
        filed
            .iter()
            .map(|title| format!("• {title}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("Subsystem: {subsystem}\nProposed:\n{proposed}\nFiled:\n{filed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_is_daily_and_repeats_after_ten_days() {
        assert_ne!(subsystem_for_day(100), subsystem_for_day(101));
        assert_eq!(subsystem_for_day(100), subsystem_for_day(110));
    }

    #[test]
    fn parses_json_embedded_in_council_output() {
        let response = parse_council_response(
            "member synthesis:\n```json\n{\"proposals\":[{\"title\":\"T\",\"description\":\"D\",\"files\":[\"a.rs\"]}],\"research_needed\":false}\n```",
        )
        .unwrap();
        assert_eq!(response.proposals[0].title, "T");
        assert!(!response.research_needed);
    }

    #[test]
    fn rejects_non_file_scoped_or_wrong_cardinality() {
        let response = CouncilResponse {
            proposals: vec![Proposal {
                title: "Vague idea".into(),
                description: "Do something".into(),
                files: vec![],
            }],
            research_needed: false,
            research_query: None,
        };
        assert!(validate_council_response(&response).is_err());
    }

    #[test]
    fn vetoed_signature_is_never_refiled() {
        let proposal = Proposal {
            title: "Improve router fallback telemetry".into(),
            description: "Add counters".into(),
            files: vec![],
        };
        let signature = proposal_signature("router", &proposal.title);
        let existing = vec![ExistingItem {
            title: proposal.title.clone(),
            signature: Some(signature.clone()),
            vetoed: true,
            open: false,
        }];
        assert!(should_skip(&proposal, &signature, &existing, &[]));
    }

    #[test]
    fn similar_open_title_is_deduplicated() {
        let proposal = Proposal {
            title: "Improve scheduler lease fairness".into(),
            description: "Tune selection".into(),
            files: vec![],
        };
        let existing = vec![ExistingItem {
            title: "Improve scheduler fairness for leases".into(),
            signature: None,
            vetoed: false,
            open: true,
        }];
        assert!(should_skip(
            &proposal,
            &proposal_signature("scheduler", &proposal.title),
            &existing,
            &[]
        ));
    }
}
