//! Model upstream revision checker (Phase 7).
//!
//! Polls the HuggingFace API for every `model_catalog` row that has
//! `upstream_source = 'huggingface'` and a non-null `upstream_id`, and
//! updates `upstream_latest_rev` + `upstream_checked_at` whenever the
//! upstream SHA changes. When a new revision lands we also flip any
//! per-computer `computer_models` row whose `last_seen_at` is more than
//! a day old into `status = 'revision_available'`, so the operator/CLI
//! can surface "please re-pull".
//!
//! Designed to run on the leader only (the scheduler wires it up that
//! way in the daemon). Defaults to a 24h interval. The first pass fires
//! ~60s after spawn so the daemon can finish booting.
//!
//! Mirrors the shape of [`crate::software_upstream::UpstreamChecker`]
//! for operational consistency (same error categories, same spawn
//! lifecycle).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// User-Agent string sent to HF.
const USER_AGENT: &str = "ForgeFleet/1.0";

/// Per-request HTTP timeout. HF API should respond in well under 10s.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Age threshold for flipping a `computer_models` row into
/// `revision_available` when its catalog row gets a new upstream SHA.
/// Rows refreshed within the last day are assumed to already match the
/// new revision (scanner just touched them).
const STALE_FILE_SECS: i64 = 24 * 3600;

/// Errors that can occur while constructing or running the checker.
#[derive(Debug, Error)]
pub enum ModelUpstreamError {
    #[error("reqwest client build failed: {0}")]
    Client(#[from] reqwest::Error),

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Report returned by [`ModelUpstreamChecker::check_all`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct UpstreamReport {
    /// Rows considered (HF-sourced, with a non-null `upstream_id`).
    pub checked: usize,
    /// Rows whose upstream SHA changed in this pass.
    pub updated: usize,
    /// Rows whose upstream SHA was already current.
    pub unchanged: usize,
    /// Rows we intentionally skipped (unsupported upstream_source etc.).
    pub skipped: usize,
    /// Per-row errors: `(catalog_id, message)`.
    pub errors: Vec<(String, String)>,
    /// How many `computer_models` rows we flipped to `revision_available`
    /// across every model during this pass.
    pub computer_rows_flagged: usize,
}

/// Upstream revision checker for `model_catalog`.
pub struct ModelUpstreamChecker {
    pg: PgPool,
}

impl ModelUpstreamChecker {
    /// Build a checker with the given Postgres pool.
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Run one pass over every eligible `model_catalog` row.
    pub async fn check_all(&self) -> Result<UpstreamReport, ModelUpstreamError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        // Optional HF token for gated repos / higher rate limit.
        let hf_token = ff_db::pg_get_secret(&self.pg, "huggingface_api_token")
            .await
            .unwrap_or(None);

        let rows = sqlx::query(
            "SELECT id, upstream_source, upstream_id, upstream_latest_rev
             FROM model_catalog
             WHERE upstream_id IS NOT NULL
             ORDER BY id",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut report = UpstreamReport {
            checked: rows.len(),
            ..UpstreamReport::default()
        };

        for row in rows {
            let id: String = row.get("id");
            let source: String = row.get("upstream_source");
            let upstream_id: String = row.get("upstream_id");
            let old_rev: Option<String> = row.get("upstream_latest_rev");

            if source != "huggingface" {
                report.skipped += 1;
                continue;
            }

            match fetch_hf_latest_sha(&http, &upstream_id, hf_token.as_deref()).await {
                Ok(new_rev) => {
                    let changed = match &old_rev {
                        Some(cur) => cur != &new_rev,
                        None => true,
                    };

                    if changed {
                        sqlx::query(
                            "UPDATE model_catalog
                                SET upstream_latest_rev = $1,
                                    upstream_checked_at = NOW()
                              WHERE id = $2",
                        )
                        .bind(&new_rev)
                        .bind(&id)
                        .execute(&self.pg)
                        .await?;

                        // Flag stale per-computer files as `revision_available`.
                        // Rows scanned within the last day are presumed fresh
                        // (the library scanner just touched them) and are left
                        // alone so we don't spam spurious alerts.
                        let flagged = sqlx::query(
                            "UPDATE computer_models
                                SET status = 'revision_available'
                              WHERE model_id = $1
                                AND status = 'ok'
                                AND last_seen_at < NOW() - make_interval(secs => $2)",
                        )
                        .bind(&id)
                        .bind(STALE_FILE_SECS as f64)
                        .execute(&self.pg)
                        .await?;

                        report.computer_rows_flagged += flagged.rows_affected() as usize;
                        report.updated += 1;
                    } else {
                        // Record that we checked even if nothing changed.
                        sqlx::query(
                            "UPDATE model_catalog
                                SET upstream_checked_at = NOW()
                              WHERE id = $1",
                        )
                        .bind(&id)
                        .execute(&self.pg)
                        .await?;

                        report.unchanged += 1;
                    }
                }
                Err(msg) => {
                    warn!(catalog_id = %id, error = %msg, "model upstream check failed");
                    report.errors.push((id, msg));
                }
            }
        }

        info!(
            checked = report.checked,
            updated = report.updated,
            unchanged = report.unchanged,
            skipped = report.skipped,
            errors = report.errors.len(),
            flagged = report.computer_rows_flagged,
            "model upstream check complete"
        );

        Ok(report)
    }

    /// Spawn a background tick that runs [`Self::check_all`] every
    /// `interval_hours`. Exits cleanly when `shutdown` flips to `true`.
    pub fn spawn(self, interval_hours: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_hours.max(1) * 3600);
        let kickoff = Duration::from_secs(60);

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.check_all().await {
                    Ok(report) => debug!(
                        checked = report.checked,
                        updated = report.updated,
                        errors = report.errors.len(),
                        "model upstream tick"
                    ),
                    Err(err) => warn!(error = %err, "model upstream tick failed"),
                }

                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        })
    }
}

/// Fetch the latest commit SHA for an HF repo id (`org/name`).
///
/// The HF model API returns the current commit at `sha` (top-level).
/// Some gated repos require an auth bearer token; when provided we
/// attach it. Non-2xx responses translate to a descriptive error.
async fn fetch_hf_latest_sha(
    http: &reqwest::Client,
    upstream_id: &str,
    token: Option<&str>,
) -> Result<String, String> {
    let url = format!("https://huggingface.co/api/models/{upstream_id}");
    let mut req = http.get(&url).header("Accept", "application/json");
    if let Some(t) = token {
        if !t.is_empty() {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
    }

    let resp = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }

    let body: JsonValue = resp
        .json()
        .await
        .map_err(|e| format!("parse JSON from {url}: {e}"))?;

    // Prefer the top-level `sha`. Fall back to the first sibling blob's
    // `lfs.oid` when HF omits the top-level sha (rare).
    if let Some(sha) = body.get("sha").and_then(|v| v.as_str()) {
        return Ok(sha.to_string());
    }

    if let Some(siblings) = body.get("siblings").and_then(|v| v.as_array()) {
        for s in siblings {
            if let Some(oid) = s
                .get("lfs")
                .and_then(|lfs| lfs.get("oid"))
                .and_then(|v| v.as_str())
            {
                return Ok(oid.to_string());
            }
        }
    }

    Err(format!("no sha/oid in {url} response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_defaults_are_zeroed() {
        let r = UpstreamReport::default();
        assert_eq!(r.checked, 0);
        assert_eq!(r.updated, 0);
        assert_eq!(r.errors.len(), 0);
    }
}
