//! Model scout (Phase 7 v1).
//!
//! Once a week, walks every entry in `fleet_task_coverage`, queries the
//! HuggingFace model API for the top-N most-downloaded models for that
//! pipeline tag, filters by license / size / denylist / existing-catalog,
//! and inserts surviving rows into `model_catalog` with
//! `lifecycle_status = 'candidate'` + `added_by = 'scout'`.
//!
//! This is a deliberately simple v1:
//!   - Discovery only; no benchmarking.
//!   - Candidates are inert until an operator runs `ff model approve <id>`.
//!   - Filters are conservative (apache/mit/openrail/llama-3/gemma) so the
//!     review queue stays small.
//!
//! The rendered row carries enough metadata (family, params, tasks,
//! license, file size if present) for the reviewer to decide without
//! opening HF.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const USER_AGENT: &str = "ForgeFleet/1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Max HF models to request per task.
const HF_LIMIT_PER_TASK: usize = 10;

/// Prefer models below this on-disk size. Larger candidates are dropped.
const MAX_CANDIDATE_SIZE_GB: f64 = 100.0;

/// Licenses we're willing to auto-promote to `candidate`. Anything else
/// gets filtered out — an operator can still add manually.
const ALLOWED_LICENSES: &[&str] = &[
    "apache-2.0",
    "mit",
    "openrail",
    "openrail++",
    "bigscience-openrail-m",
    "llama2",
    "llama-3",
    "llama-3-community",
    "llama3",
    "llama3.1",
    "llama3.2",
    "llama3.3",
    "gemma",
    "gemma-terms",
    "cc-by-4.0",
    "cc-by-sa-4.0",
    "bsd-3-clause",
    "mpl-2.0",
];

/// Default denylist path. Soft-fails if missing.
pub const DEFAULT_DENYLIST_PATH: &str =
    "/Users/venkat/projects/forge-fleet/config/scout_denylist.toml";

/// Errors that can occur during a scout pass.
#[derive(Debug, Error)]
pub enum ScoutError {
    #[error("reqwest client build failed: {0}")]
    Client(#[from] reqwest::Error),

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Outcome of one scout pass.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScoutReport {
    /// Raw HF responses we pulled down.
    pub discovered: usize,
    /// Rows added to `model_catalog` as `lifecycle_status='candidate'`.
    pub added_as_candidates: usize,
    /// Rows rejected by license/size/duplicate/deny checks.
    pub filtered_out: usize,
    /// Tasks we walked.
    pub tasks_scanned: usize,
}

/// Model scout.
pub struct ModelScout {
    pg: PgPool,
    denylist_path: PathBuf,
}

impl ModelScout {
    /// Build a scout with the given pool and the default denylist path.
    pub fn new(pg: PgPool) -> Self {
        Self {
            pg,
            denylist_path: PathBuf::from(DEFAULT_DENYLIST_PATH),
        }
    }

    /// Override the denylist path (tests, alt configs).
    pub fn with_denylist<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.denylist_path = path.as_ref().to_path_buf();
        self
    }

    /// Run one scout pass. Returns a summary — no panics; any HF/DB
    /// error for a single task is logged and skipped.
    pub async fn scout_once(&self) -> Result<ScoutReport, ScoutError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        let hf_token = ff_db::pg_get_secret(&self.pg, "huggingface_api_token")
            .await
            .unwrap_or(None);

        let denylist = load_denylist(&self.denylist_path);
        let existing = load_existing_catalog_keys(&self.pg).await?;

        let tasks: Vec<String> = sqlx::query_scalar("SELECT task FROM fleet_task_coverage")
            .fetch_all(&self.pg)
            .await?;

        let mut report = ScoutReport {
            tasks_scanned: tasks.len(),
            ..ScoutReport::default()
        };

        for task in tasks {
            match fetch_hf_models_for_task(&http, &task, hf_token.as_deref()).await {
                Ok(models) => {
                    report.discovered += models.len();
                    for m in models {
                        if let Some(entry) = evaluate(&m, &existing, &denylist) {
                            match insert_candidate(&self.pg, &entry, &task).await {
                                Ok(true) => report.added_as_candidates += 1,
                                Ok(false) => report.filtered_out += 1,
                                Err(err) => {
                                    warn!(task = %task, id = %entry.id, error = %err,
                                          "scout insert failed");
                                    report.filtered_out += 1;
                                }
                            }
                        } else {
                            report.filtered_out += 1;
                        }
                    }
                }
                Err(err) => warn!(task = %task, error = %err, "scout HF query failed"),
            }
        }

        info!(
            tasks_scanned = report.tasks_scanned,
            discovered = report.discovered,
            added = report.added_as_candidates,
            filtered = report.filtered_out,
            "scout: {} new candidate(s) awaiting review",
            report.added_as_candidates,
        );

        Ok(report)
    }

    /// Spawn a background tick that runs [`Self::scout_once`] every
    /// `interval_hours` (default production value: 168 = 1 week).
    pub fn spawn(self, interval_hours: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_hours.max(1) * 3600);
        // Kick off after 5 min so the boot rush finishes first.
        let kickoff = Duration::from_secs(300);

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.scout_once().await {
                    Ok(r) => debug!(
                        discovered = r.discovered,
                        added = r.added_as_candidates,
                        "model scout tick"
                    ),
                    Err(err) => warn!(error = %err, "model scout tick failed"),
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

/// Pulled-down subset of HF's `/api/models` response entry.
struct HfModel {
    model_id: String,
    author: Option<String>,
    license: Option<String>,
    tasks: Vec<String>,
    downloads: u64,
    size_gb: Option<f64>,
}

async fn fetch_hf_models_for_task(
    http: &reqwest::Client,
    task: &str,
    token: Option<&str>,
) -> Result<Vec<HfModel>, String> {
    let url = format!(
        "https://huggingface.co/api/models?pipeline_tag={task}&sort=downloads&limit={HF_LIMIT_PER_TASK}&full=true"
    );
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
    let arr = body.as_array().ok_or("HF response not an array")?;

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let model_id = item
            .get("modelId")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        if model_id.is_empty() {
            continue;
        }

        let author = item
            .get("author")
            .and_then(|v| v.as_str())
            .map(String::from);

        let license = extract_license(item);

        let mut tasks: Vec<String> = Vec::new();
        if let Some(pt) = item.get("pipeline_tag").and_then(|v| v.as_str()) {
            tasks.push(pt.to_string());
        }
        if let Some(tag_arr) = item.get("tags").and_then(|v| v.as_array()) {
            for t in tag_arr {
                if let Some(s) = t.as_str() {
                    // HF tags include mixed task-ish entries; keep those
                    // matching the known pipeline-tag family.
                    if looks_like_pipeline_tag(s) && !tasks.iter().any(|x| x == s) {
                        tasks.push(s.to_string());
                    }
                }
            }
        }

        let downloads = item.get("downloads").and_then(|v| v.as_u64()).unwrap_or(0);

        // Size hint: HF sometimes returns `usedStorage` or a siblings array
        // with file sizes. Fall back to a parameter-count heuristic.
        let size_gb = item
            .get("usedStorage")
            .and_then(|v| v.as_u64())
            .map(|b| b as f64 / 1_073_741_824.0);

        out.push(HfModel {
            model_id,
            author,
            license,
            tasks,
            downloads,
            size_gb,
        });
    }
    Ok(out)
}

fn extract_license(item: &JsonValue) -> Option<String> {
    if let Some(s) = item
        .get("cardData")
        .and_then(|c| c.get("license"))
        .and_then(|v| v.as_str())
    {
        return Some(s.to_string());
    }
    if let Some(s) = item.get("license").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    // HF sometimes attaches license as a `license:...` tag.
    if let Some(tags) = item.get("tags").and_then(|v| v.as_array()) {
        for t in tags {
            if let Some(s) = t.as_str() {
                if let Some(rest) = s.strip_prefix("license:") {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

fn looks_like_pipeline_tag(s: &str) -> bool {
    // Short whitelist of common pipeline tags we care about.
    matches!(
        s,
        "text-generation"
            | "code"
            | "feature-extraction"
            | "automatic-speech-recognition"
            | "image-text-to-text"
            | "text-to-speech"
            | "visual-question-answering"
            | "sentence-similarity"
            | "text-classification"
    )
}

/// Evaluated candidate ready for insertion.
struct CandidateEntry {
    id: String,
    display_name: String,
    family: String,
    license: String,
    tasks: Vec<String>,
    size_gb: Option<f64>,
    upstream_id: String,
}

fn evaluate(
    m: &HfModel,
    existing: &HashSet<String>,
    denylist: &HashSet<String>,
) -> Option<CandidateEntry> {
    let lic_raw = m.license.clone().unwrap_or_default().to_ascii_lowercase();
    if lic_raw.is_empty() {
        debug!(id = %m.model_id, "scout filter: missing license");
        return None;
    }
    if !ALLOWED_LICENSES.iter().any(|l| lic_raw == *l) {
        debug!(id = %m.model_id, license = %lic_raw, "scout filter: license not in allowlist");
        return None;
    }

    if let Some(sz) = m.size_gb {
        if sz > MAX_CANDIDATE_SIZE_GB {
            debug!(id = %m.model_id, size_gb = sz, "scout filter: oversize");
            return None;
        }
    }

    if denylist.contains(&m.model_id.to_ascii_lowercase()) {
        debug!(id = %m.model_id, "scout filter: denylist hit");
        return None;
    }

    // Synthesize a compact catalog id from org/name.
    let compact = short_id(&m.model_id);
    if existing.contains(&m.model_id) || existing.contains(&compact) {
        debug!(id = %m.model_id, "scout filter: already in catalog");
        return None;
    }

    // Weed out obvious junk: no downloads at all, or tasks empty.
    if m.downloads == 0 || m.tasks.is_empty() {
        return None;
    }

    let family = m
        .author
        .clone()
        .unwrap_or_else(|| compact.split('-').next().unwrap_or("unknown").to_string());

    Some(CandidateEntry {
        id: compact.clone(),
        display_name: m.model_id.clone(),
        family,
        license: lic_raw,
        tasks: m.tasks.clone(),
        size_gb: m.size_gb,
        upstream_id: m.model_id.clone(),
    })
}

fn short_id(hf_id: &str) -> String {
    // "org/Name-Of-Model" → "name-of-model". Collision rare enough for v1.
    let tail = hf_id.rsplit('/').next().unwrap_or(hf_id);
    let mut out = String::with_capacity(tail.len());
    for ch in tail.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

async fn load_existing_catalog_keys(pg: &PgPool) -> Result<HashSet<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT id, upstream_id FROM model_catalog")
        .fetch_all(pg)
        .await?;
    let mut set = HashSet::with_capacity(rows.len() * 2);
    for r in rows {
        let id: String = r.get("id");
        set.insert(id);
        let upstream: Option<String> = r.get("upstream_id");
        if let Some(u) = upstream {
            set.insert(u);
        }
    }
    Ok(set)
}

fn load_denylist(path: &Path) -> HashSet<String> {
    #[derive(Deserialize, Default)]
    struct DenyFile {
        #[serde(default)]
        deny: Vec<String>,
    }

    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashSet::new(),
    };
    let parsed: DenyFile = toml::from_str(&raw).unwrap_or_default();
    parsed
        .deny
        .into_iter()
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

async fn insert_candidate(
    pg: &PgPool,
    entry: &CandidateEntry,
    _origin_task: &str,
) -> Result<bool, sqlx::Error> {
    let tasks = json!(entry.tasks);
    let result = sqlx::query(
        "INSERT INTO model_catalog
             (id, display_name, family, license, tasks,
              upstream_source, upstream_id,
              file_size_gb,
              lifecycle_status, added_by)
         VALUES ($1, $2, $3, $4, $5, 'huggingface', $6, $7, 'candidate', 'scout')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&entry.id)
    .bind(&entry.display_name)
    .bind(&entry.family)
    .bind(&entry.license)
    .bind(&tasks)
    .bind(&entry.upstream_id)
    .bind(entry.size_gb)
    .execute(pg)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_normalizes_org_slash_name() {
        assert_eq!(short_id("Qwen/Qwen3-Coder-30B"), "qwen3-coder-30b");
        assert_eq!(short_id("just-a-name"), "just-a-name");
        assert_eq!(short_id("meta-llama/Llama-3.3-70B-Instruct"), "llama-3-3-70b-instruct");
    }

    #[test]
    fn denylist_load_missing_returns_empty() {
        let s = load_denylist(Path::new("/nonexistent/path/denylist.toml"));
        assert!(s.is_empty());
    }
}
