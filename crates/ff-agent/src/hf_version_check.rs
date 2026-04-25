//! Detect when a HuggingFace repo has new commits since we downloaded a model.
//!
//! Each library row stores `source_url` (e.g. `hf://Qwen/Qwen3-Coder-30B-...`).
//! We can ask HF's API for the current `main` revision SHA. If we recorded the
//! SHA at download time (in `library.sha256` for now — until we add a dedicated
//! `revision` column), we can compare and tell the user when an update exists.
//!
//! For now, we just LIST the latest revision and let the user decide whether to
//! re-download. A future enhancement: stash the revision in a new column or in
//! `params` JSONB.

use std::collections::BTreeMap;

/// One HF repo update report.
#[derive(Debug, Clone)]
pub struct HfUpdate {
    pub catalog_id: String,
    pub hf_repo: String,
    pub current_revision: String,
    pub last_modified: String,
    pub library_rows: usize,
}

/// Scan all catalog entries, query HF for the current main revision of each
/// `hf_repo` listed in their variants, and return a list of updates discovered.
/// Optional `token` (e.g. from `fleet_secrets.huggingface.token`) used for
/// gated models.
pub async fn check_catalog_updates(
    pool: &sqlx::PgPool,
    token: Option<&str>,
) -> Result<Vec<HfUpdate>, String> {
    let catalog = ff_db::pg_list_catalog(pool)
        .await
        .map_err(|e| format!("pg_list_catalog: {e}"))?;
    let library = ff_db::pg_list_library(pool, None)
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;

    // Map (catalog_id, runtime) -> count of library rows.
    let mut lib_count: BTreeMap<(String, String), usize> = BTreeMap::new();
    for r in &library {
        *lib_count
            .entry((r.catalog_id.clone(), r.runtime.clone()))
            .or_insert(0) += 1;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent("ForgeFleet/1.0")
        .build()
        .map_err(|e| format!("reqwest client: {e}"))?;

    let mut updates: Vec<HfUpdate> = Vec::new();
    for entry in &catalog {
        let variants = match entry.variants.as_array() {
            Some(v) => v,
            None => continue,
        };
        for v in variants {
            let hf_repo = match v.get("hf_repo").and_then(|x| x.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let runtime = v.get("runtime").and_then(|x| x.as_str()).unwrap_or("");

            // Only check repos we actually have on disk.
            let n_rows = lib_count
                .get(&(entry.id.clone(), runtime.to_string()))
                .copied()
                .unwrap_or(0);
            if n_rows == 0 {
                continue;
            }

            let info = match fetch_repo_info(&client, hf_repo, token).await {
                Ok(i) => i,
                Err(e) => {
                    tracing::debug!("HF info fetch {hf_repo}: {e}");
                    continue;
                }
            };

            updates.push(HfUpdate {
                catalog_id: entry.id.clone(),
                hf_repo: hf_repo.to_string(),
                current_revision: info.0,
                last_modified: info.1,
                library_rows: n_rows,
            });
        }
    }
    Ok(updates)
}

/// Returns (revision, lastModified-string) for an HF repo's main branch.
async fn fetch_repo_info(
    client: &reqwest::Client,
    repo: &str,
    token: Option<&str>,
) -> Result<(String, String), String> {
    let url = format!("https://huggingface.co/api/models/{repo}");
    let mut req = client.get(&url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let sha = json
        .get("sha")
        .and_then(|s| s.as_str())
        .unwrap_or("?")
        .to_string();
    let last = json
        .get("lastModified")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    Ok((sha, last))
}
