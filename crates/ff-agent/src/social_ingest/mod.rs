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

use anyhow::{Context, Result, anyhow};
use sqlx::PgPool;
use uuid::Uuid;

use self::analyzer::Analysis;
use self::fetcher::FetchedPost;
use self::platform::detect_platform;

/// Vision model preference (matches IDs in `config/model_catalog.toml`).
/// First hit on a healthy Pulse-advertised LLM server wins.
const VISION_MODEL_PREFS: &[&str] = &[
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
    let (endpoint, model_id) = pick_vision_server().await?;
    let analysis: Analysis = analyzer::analyze(&fetched, &endpoint, &model_id).await?;

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

/// Pick a healthy vision LLM via Pulse. Tries each model ID in preference
/// order. Returns `(endpoint_base_url, served_model_id)`.
async fn pick_vision_server() -> Result<(String, String)> {
    let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
    let reader = ff_pulse::reader::PulseReader::new(&redis_url)
        .map_err(|e| anyhow!("PulseReader::new: {e}"))?;

    for model_id in VISION_MODEL_PREFS {
        match reader.pick_llm_server_for(model_id).await {
            Ok(Some((_name, server))) => {
                // `LlmServer` carries the endpoint URL — fall back to
                // constructing one from host+port if needed.
                // TODO: once Pulse beats expose a canonical `base_url`
                // field, use it directly instead of scanning fields.
                let endpoint = extract_endpoint(&server)
                    .ok_or_else(|| anyhow!("LLM server has no resolvable endpoint"))?;
                return Ok((endpoint, (*model_id).to_string()));
            }
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(model = %model_id, error = %e, "pick_llm_server_for failed");
                continue;
            }
        }
    }
    Err(anyhow!("no vision model loaded anywhere"))
}

/// Best-effort endpoint extractor. `LlmServer` is defined in
/// `ff_pulse::beat_v2` with an `endpoint` or `base_url` field across
/// schema revisions — we probe both via `serde_json` to stay forward-
/// compatible.
fn extract_endpoint(server: &ff_pulse::beat_v2::LlmServer) -> Option<String> {
    let v = serde_json::to_value(server).ok()?;
    for key in ["base_url", "endpoint", "url"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    // Fall back to host + port if present.
    let host = v.get("host").and_then(|x| x.as_str())?;
    let port = v.get("port").and_then(|x| x.as_u64())?;
    Some(format!("http://{host}:{port}"))
}
