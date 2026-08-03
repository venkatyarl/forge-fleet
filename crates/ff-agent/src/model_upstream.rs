//! Model upstream revision checker (Phase 7).
//!
//! Polls the HuggingFace API for every non-retired `fleet_model_catalog`
//! variant that declares an `hf_repo`, and updates that variant's
//! `upstream_latest_rev` + `upstream_checked_at` metadata whenever the
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

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
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
    /// Unique Hugging Face repositories considered.
    pub checked: usize,
    /// Repositories whose upstream SHA changed in this pass.
    pub updated: usize,
    /// Repositories whose upstream SHA was already current.
    pub unchanged: usize,
    /// Catalog rows skipped because they had no usable `hf_repo` variant.
    pub skipped: usize,
    /// Optimistic catalog-update conflicts deferred to the next pass.
    pub conflicts: usize,
    /// Per-row errors: `(catalog_id, message)`.
    pub errors: Vec<(String, String)>,
    /// How many `computer_models` rows we flipped to `revision_available`
    /// across every model during this pass.
    pub computer_rows_flagged: usize,
}

/// Upstream revision checker for canonical catalog variants.
pub struct ModelUpstreamChecker {
    pg: PgPool,
    client: reqwest::Client,
}

impl ModelUpstreamChecker {
    /// Build a checker with the given Postgres pool.
    pub fn new(pg: PgPool) -> Self {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("build reqwest client");
        Self { pg, client }
    }

    /// Run one pass over every eligible canonical catalog row.
    pub async fn check_all(&self) -> Result<UpstreamReport, ModelUpstreamError> {
        let http = &self.client;

        // Optional HF token for gated repos / higher rate limit. Canonical key
        // is `huggingface.token` (what `ff secrets set` writes); the old
        // `huggingface_api_token` key was never set by operators → ran
        // unauthenticated (deep review gap #9).
        let hf_token = ff_db::pg_get_secret(&self.pg, "huggingface.token")
            .await
            .unwrap_or(None);

        let rows = sqlx::query(
            "SELECT id, variants
             FROM fleet_model_catalog
             WHERE COALESCE(lifecycle, 'active') <> 'retired'
             ORDER BY id",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut report = UpstreamReport::default();

        for row in rows {
            let id: String = row.get("id");
            let original_variants: JsonValue = row.get("variants");
            let repos = match unique_hf_repos(&original_variants) {
                Ok(repos) if !repos.is_empty() => repos,
                Ok(_) => {
                    report.skipped += 1;
                    continue;
                }
                Err(message) => {
                    report.skipped += 1;
                    report.errors.push((id, message.to_string()));
                    continue;
                }
            };

            let mut next_variants = original_variants.clone();
            let checked_at = Utc::now().to_rfc3339();
            let mut changed_repos = 0_usize;
            let mut unchanged_repos = 0_usize;
            let mut successful_repos = 0_usize;

            for upstream_id in repos {
                report.checked += 1;
                match fetch_hf_latest_sha(http, &upstream_id, hf_token.as_deref()).await {
                    Ok(new_rev) => {
                        match apply_hf_revision(
                            &mut next_variants,
                            &upstream_id,
                            &new_rev,
                            &checked_at,
                        ) {
                            Ok(true) => changed_repos += 1,
                            Ok(false) => unchanged_repos += 1,
                            Err(message) => {
                                report
                                    .errors
                                    .push((format!("{id}:{upstream_id}"), message.to_string()));
                                continue;
                            }
                        }
                        successful_repos += 1;
                    }
                    Err(message) => {
                        warn!(catalog_id = %id, hf_repo = %upstream_id, error = %message,
                              "model upstream check failed");
                        report.errors.push((format!("{id}:{upstream_id}"), message));
                    }
                }
            }

            if successful_repos == 0 {
                continue;
            }

            let mut tx = self.pg.begin().await?;
            let updated = sqlx::query(
                "UPDATE fleet_model_catalog
                    SET variants = $1,
                        updated_at = NOW()
                  WHERE id = $2
                    AND variants = $3",
            )
            .bind(&next_variants)
            .bind(&id)
            .bind(&original_variants)
            .execute(&mut *tx)
            .await?;

            if updated.rows_affected() == 0 {
                tx.rollback().await?;
                warn!(catalog_id = %id, "catalog variants changed concurrently; deferring upstream metadata update");
                report.conflicts += 1;
                report.errors.push((
                    id,
                    "catalog variants changed concurrently; retry on next pass".to_string(),
                ));
                continue;
            }

            if changed_repos > 0 {
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
                .execute(&mut *tx)
                .await?;

                report.computer_rows_flagged += flagged.rows_affected() as usize;
            }

            tx.commit().await?;
            report.updated += changed_repos;
            report.unchanged += unchanged_repos;
        }

        info!(
            checked = report.checked,
            updated = report.updated,
            unchanged = report.unchanged,
            skipped = report.skipped,
            conflicts = report.conflicts,
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

fn unique_hf_repos(variants: &JsonValue) -> Result<Vec<String>, &'static str> {
    let Some(variants) = variants.as_array() else {
        return Err("catalog variants must be a JSON array");
    };
    let mut repos = BTreeMap::new();
    for variant in variants {
        let Some(repo) = variant.get("hf_repo").and_then(JsonValue::as_str) else {
            continue;
        };
        let repo = repo.trim();
        if !repo.is_empty() {
            repos
                .entry(repo.to_ascii_lowercase())
                .or_insert_with(|| repo.to_string());
        }
    }
    Ok(repos.into_values().collect())
}

fn apply_hf_revision(
    variants: &mut JsonValue,
    hf_repo: &str,
    new_rev: &str,
    checked_at: &str,
) -> Result<bool, &'static str> {
    let Some(variants) = variants.as_array_mut() else {
        return Err("catalog variants must be a JSON array");
    };
    let mut matched = false;
    let mut changed = false;
    for variant in variants {
        let Some(object) = variant.as_object_mut() else {
            continue;
        };
        let matches_repo = object
            .get("hf_repo")
            .and_then(JsonValue::as_str)
            .is_some_and(|repo| repo.trim().eq_ignore_ascii_case(hf_repo));
        if !matches_repo {
            continue;
        }
        matched = true;
        let old_rev = object
            .get("upstream_latest_rev")
            .and_then(JsonValue::as_str)
            .filter(|revision| !revision.is_empty())
            .map(str::to_string);
        if old_rev.as_deref() != Some(new_rev) {
            if let Some(old_rev) = old_rev {
                object.insert("upstream_previous_rev".to_string(), old_rev.into());
            }
            object.insert("upstream_latest_rev".to_string(), new_rev.into());
            changed = true;
        }
        object.insert("upstream_checked_at".to_string(), checked_at.into());
    }
    matched
        .then_some(changed)
        .ok_or("hf_repo disappeared from catalog variants")
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
    if let Some(t) = token
        && !t.is_empty()
    {
        req = req.header("Authorization", format!("Bearer {t}"));
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
        assert_eq!(r.conflicts, 0);
        assert_eq!(r.errors.len(), 0);
    }

    #[test]
    fn canonical_variants_deduplicate_hf_repos_case_insensitively() {
        let variants = serde_json::json!([
            {"hf_repo": "Org/Model", "quant": "Q4_K_M"},
            {"hf_repo": "org/model", "quant": "Q8_0"},
            {"runtime": "llama.cpp"}
        ]);
        assert_eq!(unique_hf_repos(&variants).unwrap(), vec!["Org/Model"]);
        assert!(unique_hf_repos(&serde_json::json!({})).is_err());
    }

    #[test]
    fn revision_update_preserves_metadata_and_previous_revision() {
        let mut variants = serde_json::json!([{
            "hf_repo": "Org/Model",
            "quant": "Q4_K_M",
            "upstream_latest_rev": "old"
        }]);
        assert!(apply_hf_revision(&mut variants, "org/model", "new", "checked").unwrap());
        assert_eq!(variants[0]["quant"], "Q4_K_M");
        assert_eq!(variants[0]["upstream_previous_rev"], "old");
        assert_eq!(variants[0]["upstream_latest_rev"], "new");
        assert_eq!(variants[0]["upstream_checked_at"], "checked");
    }

    #[test]
    fn unchanged_revision_only_refreshes_checked_at() {
        let mut variants = serde_json::json!([{
            "hf_repo": "Org/Model",
            "upstream_latest_rev": "same"
        }]);
        assert!(!apply_hf_revision(&mut variants, "Org/Model", "same", "later").unwrap());
        assert_eq!(variants[0]["upstream_latest_rev"], "same");
        assert_eq!(variants[0]["upstream_checked_at"], "later");
        assert!(variants[0].get("upstream_previous_rev").is_none());
    }
}
