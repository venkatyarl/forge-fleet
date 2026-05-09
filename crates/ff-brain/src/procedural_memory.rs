//! Procedural Memory — Nightly Consolidation (Phase 14)
//!
//! Extracts recurring successful patterns from completed sessions and stores
//! them as reusable `agent_procedures`.  A procedure is a learned skill:
//! when a task summary matches the `trigger_pattern`, the stored `steps`
//! give the agent a head-start.
//!
//! ## Consolidation pipeline
//! 1. Scan sessions completed in the last 7 days.
//! 2. Group by keyword overlap in `goal` text (simple token-based clustering).
//! 3. For clusters with ≥3 sessions and ≥80 % success rate, extract the
//!    most common tool sequence from `agent_steps`.
//! 4. Upsert into `agent_procedures` with a regex-friendly trigger pattern.

use anyhow::Result;
use chrono::{Duration, Utc};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Minimum cluster size to consider for procedure extraction.
const MIN_CLUSTER_SIZE: usize = 3;
/// Minimum success rate (0.0–1.0) for a cluster to become a procedure.
const MIN_SUCCESS_RATE: f64 = 0.80;
/// Look-back window for session scanning (days).
const LOOKBACK_DAYS: i64 = 7;

/// A cluster of similar sessions.
#[derive(Debug)]
struct SessionCluster {
    keyword: String,
    session_ids: Vec<Uuid>,
    successful: usize,
    total: usize,
}

/// Run the nightly consolidation tick.
///
/// Returns the number of new or updated procedures.
pub async fn consolidate(pg: &PgPool) -> Result<usize> {
    let since = Utc::now() - Duration::days(LOOKBACK_DAYS);

    // 1. Fetch recent completed sessions.
    let rows = sqlx::query(
        r#"
        SELECT id, goal, status
        FROM agent_sessions
        WHERE status IN ('succeeded', 'failed')
          AND updated_at > $1
        "#,
    )
    .bind(since)
    .fetch_all(pg)
    .await?;

    #[derive(Debug)]
    struct SessionBrief {
        id: Uuid,
        goal: String,
        succeeded: bool,
    }

    let mut sessions = Vec::with_capacity(rows.len());
    for row in &rows {
        sessions.push(SessionBrief {
            id: row.get("id"),
            goal: row.get::<String, _>("goal"),
            succeeded: row.get::<String, _>("status") == "succeeded",
        });
    }

    if sessions.len() < MIN_CLUSTER_SIZE {
        debug!(count = sessions.len(), "not enough recent sessions to consolidate");
        return Ok(0);
    }

    // 2. Simple keyword clustering: tokenise goal, use most frequent non-stopword.
    let mut clusters: HashMap<String, SessionCluster> = HashMap::new();
    for s in &sessions {
        let tokens = tokenise(&s.goal);
        if tokens.is_empty() {
            continue;
        }
        // Use the first meaningful token as the cluster key.
        let keyword = tokens[0].clone();
        let cluster = clusters.entry(keyword.clone()).or_insert_with(|| SessionCluster {
            keyword: keyword.clone(),
            session_ids: Vec::new(),
            successful: 0,
            total: 0,
        });
        cluster.session_ids.push(s.id);
        cluster.total += 1;
        if s.succeeded {
            cluster.successful += 1;
        }
    }

    // 3. For viable clusters, extract tool sequences and upsert procedures.
    let mut procedures_created = 0usize;
    for cluster in clusters.values() {
        if cluster.total < MIN_CLUSTER_SIZE {
            continue;
        }
        let success_rate = cluster.successful as f64 / cluster.total as f64;
        if success_rate < MIN_SUCCESS_RATE {
            continue;
        }

        // Fetch steps for this cluster's sessions.
        let steps = extract_common_steps(pg, &cluster.session_ids).await?;
        if steps.is_empty() {
            continue;
        }

        let procedure_name = format!("auto_{}", sanitize_name(&cluster.keyword));
        let trigger_pattern = format!("(?i)\\b{}\\b", regex::escape(&cluster.keyword));
        let steps_json = serde_json::to_value(&steps)?;

        // Upsert into agent_procedures.
        let result = sqlx::query(
            r#"
            INSERT INTO agent_procedures (name, trigger_pattern, steps, success_rate, usage_count)
            VALUES ($1, $2, $3, $4, 0)
            ON CONFLICT (name) DO UPDATE
            SET steps = EXCLUDED.steps,
                success_rate = EXCLUDED.success_rate,
                created_at = NOW()
            RETURNING id
            "#,
        )
        .bind(&procedure_name)
        .bind(&trigger_pattern)
        .bind(&steps_json)
        .bind(success_rate as f32)
        .fetch_optional(pg)
        .await?;

        if result.is_some() {
            info!(
                procedure = %procedure_name,
                keyword = %cluster.keyword,
                sessions = cluster.total,
                success_rate = %format!("{:.0}%", success_rate * 100.0),
                "procedural memory consolidated"
            );
            procedures_created += 1;
        }
    }

    if procedures_created > 0 {
        info!(procedures_created, "consolidation tick complete");
    } else {
        debug!("consolidation tick: no new procedures");
    }

    Ok(procedures_created)
}

/// Extract the most common ordered step names from a set of sessions.
async fn extract_common_steps(pg: &PgPool, session_ids: &[Uuid]) -> Result<Vec<String>> {
    if session_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Build an IN clause.  sqlx doesn't directly support Vec<Uuid> IN for Postgres,
    // so we use ANY with an array.
    let ids: Vec<Uuid> = session_ids.to_vec();
    let rows = sqlx::query(
        r#"
        SELECT session_id, name
        FROM agent_steps
        WHERE session_id = ANY($1)
          AND status = 'completed'
        ORDER BY session_id, id
        "#,
    )
    .bind(&ids)
    .fetch_all(pg)
    .await?;

    // Group by session.
    let mut by_session: HashMap<Uuid, Vec<String>> = HashMap::new();
    for row in &rows {
        let sid: Uuid = row.get("session_id");
        let name: String = row.get("name");
        by_session.entry(sid).or_default().push(name);
    }

    // Find the most common sequence (allowing slight variation by using
    // the first session's sequence as a candidate and scoring others).
    let mut best: Option<(Vec<String>, usize)> = None;
    for seq in by_session.values() {
        let score = by_session.values().filter(|s| similarity(s, seq) >= 0.5).count();
        if best.as_ref().is_none_or(|(_, b)| score > *b) {
            best = Some((seq.clone(), score));
        }
    }

    Ok(best.map(|(seq, _)| seq).unwrap_or_default())
}

/// Simple token-based similarity (Jaccard-ish).
fn similarity(a: &[String], b: &[String]) -> f64 {
    let set_a: std::collections::HashSet<&String> = a.iter().collect();
    let set_b: std::collections::HashSet<&String> = b.iter().collect();
    let intersection = set_a.intersection(&set_b).count() as f64;
    let union = set_a.union(&set_b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Tokenise a goal string into lowercase keywords, stripping stopwords.
fn tokenise(goal: &str) -> Vec<String> {
    let stopwords: std::collections::HashSet<&str> = [
        "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for",
        "of", "with", "by", "from", "is", "are", "was", "were", "be", "been",
        "being", "have", "has", "had", "do", "does", "did", "will", "would",
        "could", "should", "may", "might", "must", "can", "need", "shall",
    ]
    .iter()
    .copied()
    .collect();

    goal.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase())
        .filter(|w| w.len() > 2 && !stopwords.contains(w.as_str()))
        .collect()
}

/// Make a valid identifier from a keyword.
fn sanitize_name(keyword: &str) -> String {
    keyword
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c.to_ascii_lowercase() } else { '_' })
        .take(40)
        .collect()
}

/// Spawn a background loop that runs consolidation every `interval_secs`.
/// Leader-gated via Postgres `fleet_leader_state`.
pub fn spawn_consolidation_loop(
    pg: PgPool,
    node_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE leader_name = $1
                              AND last_heartbeat > NOW() - INTERVAL '60 seconds'
                        )
                        "#
                    )
                    .bind(&node_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    if let Err(e) = consolidate(&pg).await {
                        warn!(error = %e, "procedural memory consolidation failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("procedural memory consolidation loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenise_basic() {
        let t = tokenise("Build a new REST API for user management");
        assert!(t.contains(&"build".to_string()));
        assert!(t.contains(&"rest".to_string()));
        assert!(t.contains(&"api".to_string()));
        assert!(!t.contains(&"a".to_string()));
    }

    #[test]
    fn test_sanitize_name() {
        assert_eq!(sanitize_name("hello-world"), "hello_world");
        assert_eq!(sanitize_name("UPPER"), "upper");
    }

    #[test]
    fn test_similarity() {
        let a = vec!["read".into(), "edit".into(), "test".into()];
        let b = vec!["read".into(), "edit".into(), "commit".into()];
        let s = similarity(&a, &b);
        assert!(s > 0.3 && s < 1.0);
    }
}
