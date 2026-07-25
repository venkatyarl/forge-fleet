//! ErrorMiner fix verification and regression lifecycle.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const VERIFY_WINDOW: Duration = Duration::hours(72);
const RESOLUTION_RATIO: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationDecision {
    Resolve,
    Regress,
}

fn resolve_or_regress(rate_before: f64, rate_after: f64) -> VerificationDecision {
    let threshold = rate_before * RESOLUTION_RATIO;
    if rate_after < threshold || (rate_before == 0.0 && rate_after == 0.0) {
        VerificationDecision::Resolve
    } else {
        VerificationDecision::Regress
    }
}

/// Advance every ErrorMiner signature whose fix has landed.
///
/// The pass is deliberately idempotent so the elected leader may run it on
/// every tick. State transitions and work-item reopening are guarded by the
/// signature's current state.
pub async fn run_fix_lifecycle_pass(pg: &PgPool) -> Result<()> {
    start_verification_for_deployed_fixes(pg).await?;

    let rows = sqlx::query(
        "SELECT signature, work_item_id, fix_commit_sha,
                (metadata->>'verify_started_at')::timestamptz AS verify_started_at,
                COALESCE(
                    (metadata->>'fix_merged_at')::timestamptz,
                    (metadata->>'verify_started_at')::timestamptz
                ) AS fix_merged_at
           FROM error_signatures
          WHERE state = 'verifying'
            AND (metadata->>'verify_started_at')::timestamptz <= NOW() - INTERVAL '72 hours'",
    )
    .fetch_all(pg)
    .await?;

    for row in rows {
        let signature: String = row.get("signature");
        let work_item_id: Option<Uuid> = row.try_get("work_item_id").ok().flatten();
        let fix_commit: Option<String> = row.try_get("fix_commit_sha").ok().flatten();
        let started_at: DateTime<Utc> = row.get("verify_started_at");
        let fix_merged_at: DateTime<Utc> = row.get("fix_merged_at");
        verify_signature(
            pg,
            &signature,
            work_item_id,
            fix_commit.as_deref(),
            started_at,
            fix_merged_at,
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT signature, work_item_id, fix_commit_sha
           FROM error_signatures
          WHERE state = 'resolved' AND count_24h >= 5",
    )
    .fetch_all(pg)
    .await?;
    for row in rows {
        let signature: String = row.get("signature");
        let work_item_id: Option<Uuid> = row.try_get("work_item_id").ok().flatten();
        let fix_commit: Option<String> = row.try_get("fix_commit_sha").ok().flatten();
        reopen_regression(
            pg,
            &signature,
            work_item_id,
            fix_commit.as_deref(),
            None,
            None,
        )
        .await?;
    }

    Ok(())
}

async fn start_verification_for_deployed_fixes(pg: &PgPool) -> Result<()> {
    // Versions are the ten-character embedded git SHA. The fleet versions
    // matrix defines convergence as every online daemon reporting a version
    // at least as new as the stamped fix.
    sqlx::query(
        "UPDATE error_signatures es
            SET state = 'verifying',
                metadata = jsonb_set(
                    COALESCE(metadata, '{}'::jsonb),
                    '{verify_started_at}',
                    to_jsonb(NOW()),
                    true
                )
          WHERE es.state = 'fix_merged'
            AND es.fix_commit_sha IS NOT NULL
            AND (
                SELECT MIN(cs.installed_version)
                  FROM computers c
                  JOIN computer_software cs ON cs.computer_id = c.id
                 WHERE c.status = 'online'
                   AND cs.software_id = 'forgefleetd_git'
            ) >= LEFT(es.fix_commit_sha, 10)
            AND NOT EXISTS (
                SELECT 1 FROM computers c
                 WHERE c.status = 'online'
                   AND NOT EXISTS (
                       SELECT 1 FROM computer_software cs
                        WHERE cs.computer_id = c.id
                          AND cs.software_id = 'forgefleetd_git'
                          AND cs.installed_version IS NOT NULL
                   )
            )",
    )
    .execute(pg)
    .await?;
    Ok(())
}

async fn verify_signature(
    pg: &PgPool,
    signature: &str,
    work_item_id: Option<Uuid>,
    fix_commit: Option<&str>,
    started_at: DateTime<Utc>,
    fix_merged_at: DateTime<Utc>,
) -> Result<()> {
    let before_start = fix_merged_at - VERIFY_WINDOW;
    let after_end = started_at + VERIFY_WINDOW;
    let before = digest_count(pg, signature, before_start, fix_merged_at).await? as f64 / 3.0;
    let after = digest_count(pg, signature, started_at, after_end).await? as f64 / 3.0;

    match resolve_or_regress(before, after) {
        VerificationDecision::Resolve => {
            let changed = sqlx::query(
                "UPDATE error_signatures
                    SET state = 'resolved', resolved_at = NOW(),
                        metadata = COALESCE(metadata, '{}'::jsonb) ||
                            jsonb_build_object('rate_before', $2, 'rate_after', $3)
                  WHERE signature = $1 AND state = 'verifying'",
            )
            .bind(signature)
            .bind(before)
            .bind(after)
            .execute(pg)
            .await?
            .rows_affected();
            if changed > 0 {
                let body = format!(
                    "✅ ErrorMiner resolved fix {} for work_item {}",
                    fix_commit.unwrap_or("unknown"),
                    work_item_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "unknown".into())
                );
                if let Err(error) =
                    crate::telegram::send_telegram_from_secrets(pg, "ErrorMiner", &body).await
                {
                    tracing::warn!(%error, %signature, "ErrorMiner resolve notification failed");
                }
            }
        }
        VerificationDecision::Regress => {
            reopen_regression(
                pg,
                signature,
                work_item_id,
                fix_commit,
                Some(before),
                Some(after),
            )
            .await?;
        }
    }
    Ok(())
}

async fn digest_count(
    pg: &PgPool,
    signature: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0)::bigint
           FROM fleet_log_digest
          WHERE line_class = $1
            AND day >= $2::date
            AND day < $3::date",
    )
    .bind(signature)
    .bind(start)
    .bind(end)
    .fetch_one(pg)
    .await?)
}

async fn reopen_regression(
    pg: &PgPool,
    signature: &str,
    work_item_id: Option<Uuid>,
    fix_commit: Option<&str>,
    rate_before: Option<f64>,
    rate_after: Option<f64>,
) -> Result<()> {
    let samples: Vec<String> = sqlx::query_scalar(
        "SELECT sample
           FROM fleet_log_digest
          WHERE line_class = $1 AND sample IS NOT NULL
            AND day >= COALESCE(
                (SELECT (metadata->>'verify_started_at')::timestamptz::date
                   FROM error_signatures WHERE signature = $1),
                CURRENT_DATE - 3
            )
          ORDER BY day DESC, node
          LIMIT 3",
    )
    .bind(signature)
    .fetch_all(pg)
    .await?;
    let commit = fix_commit.unwrap_or("unknown");
    let reason = format!(
        "REGRESSED after fix commit {commit}: {}",
        if samples.is_empty() {
            "no post-fix samples recorded".to_string()
        } else {
            samples.join(" | ")
        }
    );

    let mut tx = pg.begin().await?;
    let changed = sqlx::query(
        "UPDATE error_signatures
            SET state = 'regressed',
                metadata = COALESCE(metadata, '{}'::jsonb) ||
                    jsonb_strip_nulls(jsonb_build_object(
                        'rate_before', $2::double precision,
                        'rate_after', $3::double precision,
                        'regressed_at', NOW()
                    ))
          WHERE signature = $1 AND state IN ('verifying', 'resolved')",
    )
    .bind(signature)
    .bind(rate_before)
    .bind(rate_after)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if changed > 0 {
        if let Some(work_item_id) = work_item_id {
            sqlx::query(
                "UPDATE work_items
                    SET status = 'ready',
                        risk_score = COALESCE(risk_score, 0) + 10,
                        last_error = $2,
                        completed_at = NULL
                  WHERE id = $1",
            )
            .bind(work_item_id)
            .bind(&reason)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_when_after_rate_is_below_ten_percent() {
        assert_eq!(
            resolve_or_regress(100.0, 9.9),
            VerificationDecision::Resolve
        );
    }

    #[test]
    fn regresses_at_ten_percent_boundary() {
        assert_eq!(
            resolve_or_regress(100.0, 10.0),
            VerificationDecision::Regress
        );
    }

    #[test]
    fn zero_fixture_rates_resolve() {
        assert_eq!(resolve_or_regress(0.0, 0.0), VerificationDecision::Resolve);
        assert_eq!(resolve_or_regress(0.0, 1.0), VerificationDecision::Regress);
    }
}
