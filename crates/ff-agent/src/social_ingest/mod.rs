//! Social media ingest subsystem.
//!
//! Pipeline:
//!   1. Platform detection ([`platform`]).
//!   2. Fetch media + metadata via yt-dlp / ffmpeg ([`fetcher`]).
//!   3. Run vision-LLM over images/frames ([`analyzer`]).
//!   4. Persist to Postgres `social_media_posts` (schema V25).
//!   5. Send a recorded Telegram notification keyed to the ingesting session
//!      ([`crate::telegram::send_telegram_recorded`]), so an operator reply to
//!      it is routed back to that session by the leader's reply poller.
//!
//! The [`ingest`] entrypoint inserts a row in state `queued`, returns its
//! UUID immediately, and kicks off a background `tokio::spawn` that
//! walks the pipeline and updates the row to `fetching` →
//! `analyzing` → `done` or `failed`.

pub mod analyzer;
pub mod fetcher;
pub mod platform;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

/// Limit concurrent social-ingest pipelines to prevent resource exhaustion.
static INGEST_SEM: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));

use self::analyzer::Analysis;
use self::fetcher::{FetchedPost, MediaItem};
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
    let session_id = notify_session_id(post_id, ingested_by.as_deref());
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

        let error = run_pipeline(pool_task.clone(), post_id, url_task.clone())
            .await
            .err()
            .map(|e| format!("{e:#}"));
        if let Some(err) = &error {
            tracing::error!(post_id = %post_id, error = %err, "social_ingest pipeline failed");
            let _ = sqlx::query(
                "UPDATE social_media_posts SET status='failed', last_error=$2 WHERE id=$1",
            )
            .bind(post_id)
            .bind(err)
            .execute(&pool_task)
            .await;
        }

        // Recorded send: stores (chat_id, tg_message_id) → session_id in
        // `telegram_messages`, so an operator REPLY to this notification is
        // routed back to the ingesting session by the leader's reply poller.
        // Best-effort — a Telegram hiccup never fails the ingest, and the send
        // silently no-ops when Telegram isn't configured.
        let (title, body) = notification_message(&url_task, post_id, &session_id, error.as_deref());
        if let Err(e) =
            crate::telegram::send_telegram_recorded(&pool_task, &title, &body, &session_id).await
        {
            tracing::warn!(post_id = %post_id, error = %e, "social_ingest: telegram notify failed (non-fatal)");
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

    let fetched: FetchedPost = if let Some(cached) = try_load_from_cache(&url, &out_dir).await {
        tracing::info!(post_id = %post_id, url = %url, "social_ingest: using cached artifacts");
        cached
    } else {
        let fetched = fetcher::fetch(&url, &out_dir).await?;
        if let Err(e) = save_to_cache(&url, &fetched).await {
            tracing::warn!(post_id = %post_id, error = %e, "social_ingest: failed to populate cache");
        }
        fetched
    };

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
        engine: Some(crate::llm_attribution::engine_label(&model_id)),
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

/// Session a recorded Telegram notification is keyed to: the ingesting session
/// when the caller provided one (`ingested_by`), else a per-post fallback so
/// operator replies about this post still have a routable session.
fn notify_session_id(post_id: Uuid, ingested_by: Option<&str>) -> String {
    match ingested_by.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => format!("social-ingest-{post_id}"),
    }
}

/// `(title, body)` for the end-of-pipeline Telegram notification. The session
/// id is embedded in the body so the operator can see which session a reply to
/// this message will be routed to.
fn notification_message(
    url: &str,
    post_id: Uuid,
    session_id: &str,
    error: Option<&str>,
) -> (String, String) {
    let title = if error.is_some() {
        "Social ingest failed".to_string()
    } else {
        "Social ingest done".to_string()
    };
    let mut body = format!("{url}\npost: {post_id}\nsession: {session_id}");
    if let Some(e) = error {
        // Fetch/vision errors can be multi-KB tool dumps; keep the message sane.
        let trimmed: String = e.chars().take(500).collect();
        body.push_str("\nerror: ");
        body.push_str(&trimmed);
    }
    body.push_str("\nReply to this message to reach the session.");
    (title, body)
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

/// Cache directory for a given URL (SHA256 of the URL).
fn cache_dir_for(url: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let url_hash = sha256_bytes(url.as_bytes());
    PathBuf::from(home)
        .join(".forgefleet")
        .join("social_ingest")
        .join("cache")
        .join(url_hash)
}

/// Compute SHA256 of a byte slice, returning a lowercase hex string.
fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compute SHA256 of a file, returning a lowercase hex string.
fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

/// On-disk manifest describing a cached ingest result.
#[derive(Debug, Serialize, Deserialize)]
struct CachedManifest {
    url: String,
    platform: String,
    author: Option<String>,
    caption: Option<String>,
    media_items: Vec<CachedMediaItem>,
    raw_metadata: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedMediaItem {
    kind: String,
    file_name: String,
    mime: String,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame_count: Option<usize>,
    sha256: String,
}

/// Try to restore a previously fetched post from the local artifact cache.
/// Every file is SHA256-verified before use so LAN-copied cache entries cannot
/// silently corrupt the pipeline.
async fn try_load_from_cache(url: &str, out_dir: &Path) -> Option<FetchedPost> {
    let cache_dir = cache_dir_for(url);
    let manifest_path = cache_dir.join("manifest.json");
    let manifest_bytes = tokio::fs::read(&manifest_path).await.ok()?;
    let manifest: CachedManifest = serde_json::from_slice(&manifest_bytes).ok()?;

    let mut media_items = Vec::with_capacity(manifest.media_items.len());
    for item in &manifest.media_items {
        let src = cache_dir.join(&item.file_name);
        if !src.exists() {
            tracing::debug!(url = %url, file = %item.file_name, "social_ingest: cached artifact missing");
            return None;
        }

        let actual_hash = tokio::task::spawn_blocking({
            let src = src.clone();
            move || sha256_file(&src)
        })
        .await
        .ok()?
        .ok()?;
        if actual_hash != item.sha256 {
            tracing::warn!(url = %url, file = %item.file_name, "social_ingest: cached artifact SHA256 mismatch");
            return None;
        }

        let dst = out_dir.join(&item.file_name);
        tokio::fs::copy(&src, &dst).await.ok()?;
        media_items.push(MediaItem {
            kind: item.kind.clone(),
            local_path: dst.to_string_lossy().into_owned(),
            mime: item.mime.clone(),
            bytes: item.bytes,
            frame_count: item.frame_count,
        });
    }

    Some(FetchedPost {
        platform: manifest.platform,
        author: manifest.author,
        caption: manifest.caption,
        media_items,
        raw_metadata: manifest.raw_metadata,
    })
}

/// Persist a successful fetch into the local artifact cache for future reuse.
/// Writes a manifest with per-file SHA256 hashes so that LAN-copied cache
/// entries can be verified on later loads.
async fn save_to_cache(url: &str, fetched: &FetchedPost) -> Result<()> {
    let cache_dir = cache_dir_for(url);
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .with_context(|| format!("create cache dir {}", cache_dir.display()))?;

    let mut cached_items = Vec::with_capacity(fetched.media_items.len());
    for item in &fetched.media_items {
        let src = PathBuf::from(&item.local_path);
        let file_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("invalid media path: {}", item.local_path))?
            .to_string();
        let dst = cache_dir.join(&file_name);

        tokio::fs::copy(&src, &dst)
            .await
            .with_context(|| format!("copy {} to cache", src.display()))?;

        let hash = tokio::task::spawn_blocking({
            let dst = dst.clone();
            move || sha256_file(&dst)
        })
        .await
        .context("spawn sha256")?
        .with_context(|| format!("sha256 cache file {}", dst.display()))?;

        cached_items.push(CachedMediaItem {
            kind: item.kind.clone(),
            file_name,
            mime: item.mime.clone(),
            bytes: item.bytes,
            frame_count: item.frame_count,
            sha256: hash,
        });
    }

    let manifest = CachedManifest {
        url: url.to_string(),
        platform: fetched.platform.clone(),
        author: fetched.author.clone(),
        caption: fetched.caption.clone(),
        media_items: cached_items,
        raw_metadata: fetched.raw_metadata.clone(),
    };

    let manifest_path = cache_dir.join("manifest.json");
    let tmp_path = cache_dir.join("manifest.json.tmp");
    let bytes = serde_json::to_vec_pretty(&manifest).context("serialize cache manifest")?;
    tokio::fs::write(&tmp_path, bytes)
        .await
        .with_context(|| format!("write cache manifest {}", tmp_path.display()))?;
    tokio::fs::rename(&tmp_path, &manifest_path)
        .await
        .with_context(|| format!("finalize cache manifest {}", manifest_path.display()))?;

    Ok(())
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

    #[test]
    fn notification_carries_session_id_for_reply_routing() {
        let post_id = Uuid::nil();

        // Explicit ingesting session wins; blank/missing falls back per-post.
        assert_eq!(
            notify_session_id(post_id, Some(" mac-forge-fleet ")),
            "mac-forge-fleet"
        );
        assert_eq!(
            notify_session_id(post_id, Some("   ")),
            format!("social-ingest-{post_id}")
        );
        assert_eq!(
            notify_session_id(post_id, None),
            format!("social-ingest-{post_id}")
        );

        let url = "https://x.com/user/status/1";
        let (title, body) = notification_message(url, post_id, "mac-forge-fleet", None);
        assert_eq!(title, "Social ingest done");
        assert!(body.contains("session: mac-forge-fleet"));
        assert!(body.contains(&post_id.to_string()));
        assert!(body.contains(url));

        let (title, body) = notification_message(url, post_id, "mac-forge-fleet", Some("boom"));
        assert_eq!(title, "Social ingest failed");
        assert!(body.contains("error: boom"));
        assert!(body.contains("session: mac-forge-fleet"));
    }

    #[test]
    fn cache_lookup_uses_artifacts_and_rejects_corrupted_lan_copies() {
        use std::sync::Mutex;
        static HOME_LOCK: Mutex<()> = Mutex::new(());

        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: serialized by HOME_LOCK so this process-global env mutation
        // does not race with other HOME-dependent tests.
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let url = "https://example.com/cache-test";
            let out_dir = tmp.path().join("out");
            tokio::fs::create_dir_all(&out_dir).await.unwrap();

            let fetched = FetchedPost {
                platform: "twitter".into(),
                author: Some("author".into()),
                caption: Some("caption".into()),
                media_items: vec![MediaItem {
                    kind: "image".into(),
                    local_path: out_dir.join("img.jpg").to_string_lossy().into_owned(),
                    mime: "image/jpeg".into(),
                    bytes: 4,
                    frame_count: None,
                }],
                raw_metadata: serde_json::json!({"id": "abc"}),
            };
            tokio::fs::write(&fetched.media_items[0].local_path, b"data")
                .await
                .unwrap();

            save_to_cache(url, &fetched).await.unwrap();

            // Cache hit returns the post and copies artifacts to out_dir.
            let cached = try_load_from_cache(url, &out_dir).await.unwrap();
            assert_eq!(cached.platform, "twitter");
            assert_eq!(cached.media_items.len(), 1);
            assert_eq!(cached.author, Some("author".into()));
            assert!(
                tokio::fs::metadata(&cached.media_items[0].local_path)
                    .await
                    .is_ok()
            );

            // Simulate a LAN-copied corrupted cache entry.
            let cache_dir = cache_dir_for(url);
            tokio::fs::write(cache_dir.join("img.jpg"), b"bad!")
                .await
                .unwrap();

            // Corrupted artifact must be rejected by SHA256 verification.
            assert!(try_load_from_cache(url, &out_dir).await.is_none());
        });
    }
}
