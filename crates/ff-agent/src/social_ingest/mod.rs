//! Social media ingest subsystem.
//!
//! Pipeline:
//!   1. Platform detection ([`platform`]).
//!   2. Fetch media + metadata via yt-dlp / ffmpeg ([`fetcher`]).
//!   3. Run vision-LLM over images/frames ([`analyzer`]).
//!   4. Persist to Postgres `social_media_posts` (schema V25).
//!
//! The [`ingest`] entrypoint inserts a row in state `queued`, returns its
//! UUID immediately, and kicks off a background `tokio::spawn` that
//! walks the pipeline and updates the row to `fetching` →
//! `analyzing` → `done` or `failed`.

pub mod analyzer;
pub mod fetcher;
pub mod platform;

use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

/// Limit concurrent social-ingest pipelines to prevent resource exhaustion.
static INGEST_SEM: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));

use self::analyzer::Analysis;
use self::fetcher::FetchedPost;
use self::platform::detect_platform;

/// Vision model preference (matches IDs in `config/model_catalog.toml`).
/// First hit on a healthy Pulse-advertised LLM server wins. qwen3-vl-30b leads
/// because it's what the fleet actually serves today — the older qwen2-vl/llava
/// IDs were all undeployed, so social_ingest had no vision server to pick and
/// silently failed every ingest at the analyze step.
const VISION_MODEL_PREFS: &[&str] = &[
    "qwen3-vl-30b-a3b",
    "qwen2-vl-7b-instruct",
    "qwen2-vl-7b",
    "llava-onevision-qwen2-7b-si",
    "llama32-vision-11b",
];

/// Kick off an ingest for a URL. Returns the new row's UUID immediately;
/// the pipeline continues in a detached tokio task.
pub async fn ingest(pool: PgPool, url: String, ingested_by: Option<String>) -> Result<Uuid> {
    let platform = detect_platform(&url).as_str();
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO social_media_posts (url, platform, status, ingested_by) \
         VALUES ($1, $2, 'queued', $3) RETURNING id",
    )
    .bind(&url)
    .bind(platform)
    .bind(ingested_by.as_deref())
    .fetch_one(&pool)
    .await
    .context("insert social_media_posts")?;
    let post_id = row.0;

    let pool_task = pool.clone();
    let url_task = url.clone();
    tokio::spawn(async move {
        // Acquire backpressure semaphore; if full, pipeline waits.
        let _permit = match INGEST_SEM.acquire().await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(post_id = %post_id, error = %e, "social_ingest semaphore closed");
                let _ = sqlx::query(
                    "UPDATE social_media_posts SET status='failed', last_error=$2 WHERE id=$1",
                )
                .bind(post_id)
                .bind("ingest semaphore closed".to_string())
                .execute(&pool_task)
                .await;
                return;
            }
        };

        if let Err(e) = run_pipeline(pool_task.clone(), post_id, url_task).await {
            tracing::error!(post_id = %post_id, error = %e, "social_ingest pipeline failed");
            let _ = sqlx::query(
                "UPDATE social_media_posts SET status='failed', last_error=$2 WHERE id=$1",
            )
            .bind(post_id)
            .bind(format!("{e:#}"))
            .execute(&pool_task)
            .await;
        }
    });

    Ok(post_id)
}

/// Full fetch → analyze → persist pipeline. Runs in a detached task.
async fn run_pipeline(pool: PgPool, post_id: Uuid, url: String) -> Result<()> {
    // ── fetching ─────────────────────────────────────────────────────
    update_status(&pool, post_id, "fetching", None).await?;
    let out_dir = post_workdir(post_id);
    tokio::fs::create_dir_all(&out_dir)
        .await
        .with_context(|| format!("mkdir {}", out_dir.display()))?;
    let fetched: FetchedPost = fetcher::fetch(&url, &out_dir).await?;

    // Persist media + caption + author before analysis so partial rows
    // are useful even if analysis later fails.
    let media_json =
        serde_json::to_value(&fetched.media_items).unwrap_or(serde_json::Value::Array(vec![]));
    sqlx::query(
        "UPDATE social_media_posts \
         SET author=$2, caption=$3, media_items=$4 \
         WHERE id=$1",
    )
    .bind(post_id)
    .bind(fetched.author.as_deref())
    .bind(fetched.caption.as_deref())
    .bind(&media_json)
    .execute(&pool)
    .await
    .context("update post post-fetch")?;

    // ── analyzing ────────────────────────────────────────────────────
    update_status(&pool, post_id, "analyzing", None).await?;
    let (endpoint, model_id) = pick_vision_server(&pool).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("build reqwest client")?;
    let analysis: Analysis = analyzer::analyze(&client, &fetched, &endpoint, &model_id).await?;

    // ── done ─────────────────────────────────────────────────────────
    let analysis_json = serde_json::to_value(&analysis)
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    sqlx::query(
        "UPDATE social_media_posts \
         SET status='done', \
             analysis=$2, \
             extracted_text=$3, \
             analyzed_at=NOW() \
         WHERE id=$1",
    )
    .bind(post_id)
    .bind(&analysis_json)
    .bind(&analysis.ocr_combined)
    .execute(&pool)
    .await
    .context("update post done")?;

    // Log the vision-analysis turn to ff_interactions (training corpus) — the
    // last LLM-dispatch path not feeding it (after council #442, research
    // #447/#451/#454, offload #448). The image(s) -> structured analysis pair is
    // vision-model training signal. Best-effort; never fails the ingest.
    let rec = ff_db::InteractionRecord {
        channel: "social_ingest".to_string(),
        request_text: format!(
            "analyze {} media item(s) from {url}",
            fetched.media_items.len()
        )
        .chars()
        .take(16000)
        .collect(),
        engine: Some(model_id.clone()),
        response_text: analysis_json.to_string().chars().take(16000).collect(),
        outcome: "success".to_string(),
        endpoint: Some(endpoint.clone()),
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(&pool, &rec).await {
        tracing::warn!(error = %e, "social_ingest: failed to log interaction (non-fatal)");
    }
    Ok(())
}

async fn update_status(
    pool: &PgPool,
    post_id: Uuid,
    status: &str,
    err: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE social_media_posts \
         SET status=$2, last_error=COALESCE($3, last_error) \
         WHERE id=$1",
    )
    .bind(post_id)
    .bind(status)
    .bind(err)
    .execute(pool)
    .await
    .context("update status")?;
    Ok(())
}

/// Working directory for a given post's artifacts.
fn post_workdir(post_id: Uuid) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".forgefleet")
        .join("social_ingest")
        .join(post_id.to_string())
}

/// Pick a healthy vision LLM via the shared DB router ([`ff_db::pg_route_deployments`]).
///
/// This replaces the old Pulse/Redis scan. `pg_route_deployments` is the single
/// scored selector every other live-dispatch path uses (agent endpoint, offload,
/// research fan-out, the `fleet_route` MCP tool), so vision routing now inherits
/// the same `healthy`-only filter, health-freshness floor (a wedged host can't
/// flip its own deployments unhealthy), least-loaded tiebreak, and workload
/// synonym tolerance (`vision`/`multimodal`) instead of a parallel scanner that
/// could drift. Returns `(endpoint_base_url, served_model_id)`.
///
/// The router already orders candidates tier→load→freshness; we still honor
/// [`VISION_MODEL_PREFS`] as a *soft* preference so the fleet's best-known vision
/// model wins when several are healthy, falling back to the top-scored candidate.
async fn pick_vision_server(pool: &PgPool) -> Result<(String, String)> {
    let filter = ff_db::RouteFilter {
        workload: Some("vision".to_string()),
        // Vision analysis is a single-shot completion, not a tool-agent loop, so
        // tool_calling is not required and no per-slot ctx floor is imposed.
        max_health_age_sec: Some(ff_db::queries::DISPATCH_HEALTH_MAX_AGE_SEC),
        prefer_least_loaded: true,
        limit: 8,
        ..Default::default()
    };
    let candidates = ff_db::pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow!("pg_route_deployments(vision): {e}"))?;
    if candidates.is_empty() {
        return Err(anyhow!("no healthy vision model deployed anywhere"));
    }

    // Soft preference over the already-scored set: first candidate whose catalog
    // id/name matches our preference order, else the top-scored one.
    let pick = VISION_MODEL_PREFS
        .iter()
        .find_map(|pref| {
            candidates.iter().find(|c| {
                model_matches(c.catalog_id.as_deref(), pref)
                    || model_matches(c.catalog_name.as_deref(), pref)
            })
        })
        .unwrap_or(&candidates[0]);

    let model_id = pick
        .catalog_id
        .clone()
        .or_else(|| pick.catalog_name.clone())
        .ok_or_else(|| anyhow!("vision candidate has no catalog id/name"))?;
    Ok((pick.endpoint.clone(), model_id))
}

/// Loose case-insensitive match of a candidate's catalog id/name against a
/// preference key — exact or substring either direction, so `qwen3-vl-30b-a3b`
/// matches a served id like `qwen3-vl-30b-a3b-instruct` and vice-versa.
fn model_matches(candidate: Option<&str>, pref: &str) -> bool {
    let Some(c) = candidate else { return false };
    let (c, p) = (c.to_ascii_lowercase(), pref.to_ascii_lowercase());
    c == p || c.contains(&p) || p.contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_matches_is_case_insensitive_and_bidirectional() {
        // exact
        assert!(model_matches(Some("qwen3-vl-30b-a3b"), "qwen3-vl-30b-a3b"));
        // case-insensitive
        assert!(model_matches(Some("Qwen3-VL-30B-A3B"), "qwen3-vl-30b-a3b"));
        // served id is a superset of the preference key
        assert!(model_matches(
            Some("qwen3-vl-30b-a3b-instruct"),
            "qwen3-vl-30b-a3b"
        ));
        // preference key is a superset of the served id
        assert!(model_matches(Some("qwen2-vl-7b"), "qwen2-vl-7b-instruct"));
        // no false positives across distinct families
        assert!(!model_matches(Some("llama32-vision-11b"), "qwen2-vl-7b"));
        // None never matches
        assert!(!model_matches(None, "qwen3-vl-30b-a3b"));
    }
}
