use std::collections::HashMap;

use anyhow::Context;
use sqlx::{PgPool, Row};
use tracing::info;

const DEFAULT_WINDOW_HOURS: i64 = 168;
const MAX_ROWS_TO_MINE: i64 = 5_000;
const MAX_SIGNALS: usize = 10;
const BUCKET_WORDS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectNeedSignal {
    pub project_id: String,
    pub theme: String,
    pub occurrences: i64,
    pub sample: String,
}

#[derive(Debug, Clone)]
struct InteractionNeedRow {
    request_text: String,
    outcome: String,
    error_text: Option<String>,
}

#[derive(Debug, Clone)]
struct BucketStats {
    occurrences: i64,
    sample: String,
    has_failure: bool,
}

/// Observe recurring needs in a project's recent interaction log.
///
/// OBSERVE only: this function is deliberately read-only. Later capability
/// synthesis/build steps should consume these signals instead of creating
/// artifacts here.
pub async fn observe_project_needs(
    pg: &PgPool,
    project_id: &str,
    window_hours: i64,
) -> anyhow::Result<Vec<ProjectNeedSignal>> {
    let window_hours = if window_hours > 0 {
        window_hours
    } else {
        DEFAULT_WINDOW_HOURS
    };

    let rows = fetch_interaction_need_rows(pg, project_id, window_hours)
        .await
        .with_context(|| format!("observe capability needs for project {project_id}"))?;

    Ok(bucket_project_need_rows(project_id, rows))
}

/// Synthesis/build placeholder for the Capability Synthesis pillar.
///
/// TODO(synth): turn stable need signals into proposed skills/tools/agents.
/// TODO(build): create artifacts only after the synthesis policy and review path
/// are defined.
pub async fn synthesize_capabilities(pg: &PgPool, project_id: &str) -> anyhow::Result<usize> {
    let signals = observe_project_needs(pg, project_id, DEFAULT_WINDOW_HOURS).await?;

    for signal in signals.iter().take(5) {
        info!(
            project_id = %signal.project_id,
            theme = %signal.theme,
            occurrences = signal.occurrences,
            sample = %signal.sample,
            "capability synthesis: observed project need signal"
        );
    }

    info!(
        project_id = %project_id,
        need_signals = signals.len(),
        "capability synthesis: {} need-signals observed (synth step TODO)",
        signals.len()
    );

    Ok(0)
}

async fn fetch_interaction_need_rows(
    pg: &PgPool,
    project_id: &str,
    window_hours: i64,
) -> anyhow::Result<Vec<InteractionNeedRow>> {
    if ff_interactions_has_project_id(pg).await? {
        fetch_interaction_need_rows_by_project_column(pg, project_id, window_hours).await
    } else {
        fetch_interaction_need_rows_by_request_meta(pg, project_id, window_hours).await
    }
}

async fn ff_interactions_has_project_id(pg: &PgPool) -> anyhow::Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name = 'ff_interactions'
               AND column_name = 'project_id'
        )",
    )
    .fetch_one(pg)
    .await
    .context("check ff_interactions.project_id column")
}

async fn fetch_interaction_need_rows_by_project_column(
    pg: &PgPool,
    project_id: &str,
    window_hours: i64,
) -> anyhow::Result<Vec<InteractionNeedRow>> {
    let rows = sqlx::query(
        "SELECT request_text, outcome, error_text
           FROM ff_interactions
          WHERE project_id = $1
            AND ts >= NOW() - ($2::text || ' hours')::interval
          ORDER BY ts DESC
          LIMIT $3",
    )
    .bind(project_id)
    .bind(window_hours)
    .bind(MAX_ROWS_TO_MINE)
    .fetch_all(pg)
    .await
    .context("fetch ff_interactions rows by project_id")?;

    Ok(rows
        .into_iter()
        .map(|row| InteractionNeedRow {
            request_text: row.get("request_text"),
            outcome: row.get("outcome"),
            error_text: row.try_get("error_text").ok().flatten(),
        })
        .collect())
}

async fn fetch_interaction_need_rows_by_request_meta(
    pg: &PgPool,
    project_id: &str,
    window_hours: i64,
) -> anyhow::Result<Vec<InteractionNeedRow>> {
    let rows = sqlx::query(
        "SELECT request_text, outcome, error_text
           FROM ff_interactions
          WHERE request_meta->>'project_id' = $1
            AND ts >= NOW() - ($2::text || ' hours')::interval
          ORDER BY ts DESC
          LIMIT $3",
    )
    .bind(project_id)
    .bind(window_hours)
    .bind(MAX_ROWS_TO_MINE)
    .fetch_all(pg)
    .await
    .context("fetch ff_interactions rows by request_meta project_id")?;

    Ok(rows
        .into_iter()
        .map(|row| InteractionNeedRow {
            request_text: row.get("request_text"),
            outcome: row.get("outcome"),
            error_text: row.try_get("error_text").ok().flatten(),
        })
        .collect())
}

fn bucket_project_need_rows(
    project_id: &str,
    rows: impl IntoIterator<Item = InteractionNeedRow>,
) -> Vec<ProjectNeedSignal> {
    let mut buckets: HashMap<String, BucketStats> = HashMap::new();

    for row in rows {
        let failed = row.outcome == "error" || row.error_text.as_deref().is_some_and(has_text);
        let theme = request_theme_bucket(&row.request_text);

        let entry = buckets.entry(theme).or_insert_with(|| BucketStats {
            occurrences: 0,
            sample: truncate_sample(&row.request_text),
            has_failure: false,
        });
        entry.occurrences += 1;
        entry.has_failure |= failed;
        if entry.sample.is_empty() {
            entry.sample = truncate_sample(&row.request_text);
        }
    }

    let mut signals: Vec<ProjectNeedSignal> = buckets
        .into_iter()
        .filter(|(_, stats)| stats.has_failure || stats.occurrences > 1)
        .map(|(theme, stats)| ProjectNeedSignal {
            project_id: project_id.to_string(),
            theme,
            occurrences: stats.occurrences,
            sample: stats.sample,
        })
        .collect();

    signals.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| a.theme.cmp(&b.theme))
    });
    signals.truncate(MAX_SIGNALS);
    signals
}

fn request_theme_bucket(request_text: &str) -> String {
    let words: Vec<String> = request_text
        .split_whitespace()
        .filter_map(normalize_bucket_word)
        .take(BUCKET_WORDS)
        .collect();

    if words.is_empty() {
        "empty-request".to_string()
    } else {
        words.join(" ")
    }
}

fn normalize_bucket_word(word: &str) -> Option<String> {
    let normalized: String = word
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .flat_map(char::to_lowercase)
        .collect();

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn truncate_sample(text: &str) -> String {
    text.trim().chars().take(500).collect()
}

fn has_text(text: &str) -> bool {
    !text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bucket_helper_normalizes_first_words() {
        assert_eq!(
            request_theme_bucket("Build: the Capability Synthesis OBSERVE step, please."),
            "build the capability synthesis observe step please"
        );
    }

    #[test]
    fn capability_bucket_helper_groups_failures_and_repeats() {
        let rows = vec![
            InteractionNeedRow {
                request_text: "Fix cargo test failure in ff-agent".to_string(),
                outcome: "error".to_string(),
                error_text: Some("compile failed".to_string()),
            },
            InteractionNeedRow {
                request_text: "Add project dashboard filter".to_string(),
                outcome: "ok".to_string(),
                error_text: None,
            },
            InteractionNeedRow {
                request_text: "Add project dashboard filter".to_string(),
                outcome: "ok".to_string(),
                error_text: None,
            },
        ];

        let signals = bucket_project_need_rows("project-1", rows);

        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].theme, "add project dashboard filter");
        assert_eq!(signals[0].occurrences, 2);
        assert!(
            signals
                .iter()
                .any(|s| s.theme == "fix cargo test failure in ff-agent")
        );
    }
}
