//! Fleet task-coverage guard (Phase 7).
//!
//! Keeps the fleet "always-on" for the set of tasks the operator declared
//! required in `fleet_task_coverage`. For each row the guard:
//!
//!   1. Counts how many currently-active deployments serve the task. A
//!      deployment serves a task if its (normalized) id matches an `active`
//!      `model_catalog` row tagged with the task, OR matches a model the
//!      operator named in the task's `preferred_model_ids` — the latter
//!      overrides stale/missing catalog tags. See `tally_task_coverage`.
//!   2. If the count is below `min_models_loaded`, picks the best catalog
//!      candidate (flagship → standard, preferring smaller/Q4 quants so
//!      a 32GB box can run it) and enqueues a deferred `ff model load`
//!      shell task targeted at a computer that can host it.
//!   3. If no viable candidate exists, records a gap. Gaps are
//!      de-spammed: the same task is only surfaced once per hour.
//!
//! Runs every 15 minutes by default on the elected leader only.
//! The `PulseReader` field is reserved for future per-computer liveness
//! filtering; today we fall back to the DB snapshot (status='online')
//! when the reader isn't wired, so this module is usable even without
//! Redis.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ff_core::model_id::normalize_model_id;
use ff_pulse::reader::PulseReader;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How long to silence a repeat gap alert for the same task (1h).
const GAP_ALERT_COOLDOWN: Duration = Duration::from_secs(3600);

/// Errors that can occur while running the guard.
#[derive(Debug, Error)]
pub enum CoverageError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// One row returned by [`CoverageGuard::check_once`] for a task that
/// cannot currently be satisfied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageGap {
    pub task: String,
    pub min_required: i32,
    pub currently_loaded: i32,
    /// Catalog ids that *could* serve the task (smallest/best quant first),
    /// even if no computer can host them right now.
    pub candidates: Vec<String>,
}

/// Result of one guard pass.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub tasks_required: usize,
    pub tasks_covered: usize,
    pub gaps: Vec<CoverageGap>,
    /// Catalog ids for which we enqueued a load-this-now deferred task.
    pub auto_loaded: Vec<String>,
}

/// Fleet task-coverage guard.
///
/// Clone-on-spawn friendly — holds an `Arc<Mutex<_>>` for the alert
/// dedup table so `spawn()` can move the guard while `check_once()`
/// continues to be callable from CLI handlers.
#[derive(Clone)]
pub struct CoverageGuard {
    pg: PgPool,
    #[allow(dead_code)]
    pulse: Option<Arc<PulseReader>>,
    last_alerted: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
}

impl CoverageGuard {
    /// Build a guard with the given Postgres pool and an optional
    /// pulse reader. `pulse` is currently only used as a future hook
    /// for liveness-aware scheduling.
    pub fn new(pg: PgPool, pulse: Option<Arc<PulseReader>>) -> Self {
        Self {
            pg,
            pulse,
            last_alerted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a guard without a pulse reader (DB-only mode).
    pub fn new_dbonly(pg: PgPool) -> Self {
        Self::new(pg, None)
    }

    /// Run one coverage pass **with remediation**: gaps that have a viable
    /// candidate host are auto-loaded via an enqueued deferred task. This is
    /// the behavior the background tick wants. See the module docs.
    pub async fn check_once(&self) -> Result<CoverageReport, CoverageError> {
        self.run_once(true).await
    }

    /// Run one coverage pass **read-only**: analyze coverage and report gaps
    /// without enqueuing any auto-load tasks. Used by the `ff model coverage`
    /// CLI so a status check has no side effects (no fleet-wide model loads,
    /// no defer-queue writes). A gap that *could* be auto-loaded is still
    /// reported as a gap (with its candidate list) instead of being silently
    /// remediated.
    pub async fn report_once(&self) -> Result<CoverageReport, CoverageError> {
        self.run_once(false).await
    }

    /// Run one coverage pass. When `remediate` is true, viable gaps are
    /// auto-loaded (enqueued); when false the pass is purely observational.
    /// See the module docs for the algorithm.
    async fn run_once(&self, remediate: bool) -> Result<CoverageReport, CoverageError> {
        let required = sqlx::query(
            "SELECT task, min_models_loaded, preferred_model_ids, priority
             FROM fleet_task_coverage
             ORDER BY
               CASE priority
                 WHEN 'critical' THEN 0
                 WHEN 'normal' THEN 1
                 WHEN 'nice-to-have' THEN 2
                 ELSE 3
               END,
               task",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut report = CoverageReport {
            tasks_required: required.len(),
            ..CoverageReport::default()
        };

        // Resolve how many active deployments serve each task ONCE, up front.
        // `computer_model_deployments.model_id` stores the deployment's
        // free-text identifier — a GGUF filename (`gemma-4-31B-it-Q4_K_M.gguf`)
        // or a runtime model name (`qwen3.6-35b-a3b`) — NOT the `model_catalog`
        // id (`gemma4-31b-it`). The previous per-task `mc.id = d.model_id`
        // join therefore never matched and coverage always reported
        // `covered=0`. We instead normalize both sides through the canonical
        // `normalize_model_id` (shared with gateway routing + pulse) and match
        // on a separator boundary. See `deployed_task_counts`.
        let deployed_counts = self.deployed_task_counts().await?;

        for row in required {
            let task: String = row.get("task");
            let min_required: i32 = row.get("min_models_loaded");
            let preferred_json: serde_json::Value = row.get("preferred_model_ids");
            let preferred_ids: Vec<String> = preferred_json
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // How many active deployments cover this task right now?
            // (Resolved up front in `deployed_task_counts` via canonical
            // model-id normalization — see the comment above the loop.)
            let currently_loaded: i32 = deployed_counts.get(&task).copied().unwrap_or(0);

            if currently_loaded >= min_required {
                report.tasks_covered += 1;
                continue;
            }

            // Find candidate catalog rows that *could* cover this task.
            let candidates = self.rank_candidates(&task, &preferred_ids).await?;

            // Try to auto-load the best candidate on any suitable computer.
            // Skipped entirely in read-only mode (`report_once`), so a status
            // check never enqueues a fleet-wide model load.
            let mut enqueued = None;
            if remediate {
                for cand in &candidates {
                    match self.pick_host_for(&cand.id, cand.min_vram_gb).await? {
                        Some(host) => {
                            let defer_id = self.enqueue_load(&cand.id, &host).await?;
                            info!(
                                task = %task,
                                model = %cand.id,
                                host = %host,
                                defer_id = %defer_id,
                                "coverage guard enqueued auto-load"
                            );
                            report.auto_loaded.push(cand.id.clone());
                            enqueued = Some(cand.id.clone());
                            break;
                        }
                        None => continue,
                    }
                }
            }

            if enqueued.is_none() {
                // Dedup: only surface the same gap once per hour. The alert
                // only makes sense when we tried (and failed) to remediate;
                // a read-only pass just reports the gap silently.
                if remediate && self.should_alert(&task).await {
                    warn!(
                        task = %task,
                        min_required,
                        currently_loaded,
                        "coverage gap — no viable candidate host available"
                    );
                }
                report.gaps.push(CoverageGap {
                    task,
                    min_required,
                    currently_loaded,
                    candidates: candidates.into_iter().map(|c| c.id).collect(),
                });
            } else {
                // Auto-load enqueued — count as covered once it lands.
                report.tasks_covered += 1;
            }
        }

        info!(
            tasks_required = report.tasks_required,
            tasks_covered = report.tasks_covered,
            gaps = report.gaps.len(),
            auto_loaded = report.auto_loaded.len(),
            "coverage guard pass complete"
        );

        Ok(report)
    }

    /// Count, per HF task, how many currently-active deployments serve it.
    ///
    /// Deployments record a free-text `model_id` (GGUF filename or runtime
    /// model name), so they cannot be joined to `model_catalog.id` directly.
    /// We normalize every active deployment id, every **active** catalog id,
    /// and every operator-declared preferred id through [`normalize_model_id`]
    /// (the same canonical form the gateway router and pulse reader use) and
    /// match on a separator boundary. A deployment serves a task if EITHER its
    /// id matches an active catalog row that lists the task, OR it matches a
    /// model the operator named in that task's `preferred_model_ids` — see
    /// [`tally_task_coverage`] for the full rule.
    async fn deployed_task_counts(&self) -> Result<HashMap<String, i32>, sqlx::Error> {
        let dep_rows =
            sqlx::query("SELECT model_id FROM computer_model_deployments WHERE status = 'active'")
                .fetch_all(&self.pg)
                .await?;

        // Only `active` catalog rows count toward coverage. `candidate` rows
        // are unreviewed model-scout discoveries whose `tasks` come straight
        // from the HF `pipeline_tag` and are frequently mislabeled (e.g. the
        // text/code MoE `qwen3-6-35b-a3b` was scouted as `image-text-to-text`),
        // so crediting them produces false coverage. `deprecated` rows are on
        // the way out. Operator-blessed `active` rows are the source of truth.
        let cat_rows =
            sqlx::query("SELECT id, tasks FROM model_catalog WHERE lifecycle_status = 'active'")
                .fetch_all(&self.pg)
                .await?;

        let pref_rows = sqlx::query("SELECT task, preferred_model_ids FROM fleet_task_coverage")
            .fetch_all(&self.pg)
            .await?;

        let deploy_norm: Vec<String> = dep_rows
            .iter()
            .map(|r| normalize_model_id(&r.get::<String, _>("model_id")))
            .collect();

        // (normalized catalog id, tasks) for each active catalog row.
        let catalog: Vec<(String, Vec<String>)> = cat_rows
            .iter()
            .map(|r| {
                let id: String = r.get("id");
                (normalize_model_id(&id), json_str_array(r.get("tasks")))
            })
            .collect();

        // (task, [normalized preferred model ids]) from the operator-curated
        // coverage table. These are an explicit "this model serves this task"
        // declaration that overrides stale/missing catalog `tasks` tags.
        let preferred: Vec<(String, Vec<String>)> = pref_rows
            .iter()
            .map(|r| {
                let task: String = r.get("task");
                let ids: Vec<String> = json_str_array(r.get("preferred_model_ids"))
                    .iter()
                    .map(|id| normalize_model_id(id))
                    .collect();
                (task, ids)
            })
            .collect();

        Ok(tally_task_coverage(&deploy_norm, &catalog, &preferred))
    }

    /// Spawn a background tick that runs [`Self::check_once`] every
    /// `interval_mins`. Exits when `shutdown` flips to `true`.
    pub fn spawn(self, interval_mins: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_mins.max(1) * 60);
        let kickoff = Duration::from_secs(90);

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.check_once().await {
                    Ok(report) => debug!(
                        tasks_required = report.tasks_required,
                        tasks_covered = report.tasks_covered,
                        gaps = report.gaps.len(),
                        auto_loaded = report.auto_loaded.len(),
                        "coverage guard tick"
                    ),
                    Err(err) => warn!(error = %err, "coverage guard tick failed"),
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

    /// Return true iff we haven't alerted about this task in the last
    /// hour; updates the dedup table as a side effect when we do alert.
    async fn should_alert(&self, task: &str) -> bool {
        let now = Utc::now();
        let mut table = self.last_alerted.lock().await;
        if let Some(last) = table.get(task) {
            let elapsed = now.signed_duration_since(*last);
            if elapsed
                < chrono::Duration::from_std(GAP_ALERT_COOLDOWN).unwrap_or(chrono::Duration::zero())
            {
                return false;
            }
        }
        table.insert(task.to_string(), now);
        true
    }

    /// Rank candidate catalog rows for a task. Preferred ids from the
    /// coverage row sort first; then `quality_tier='flagship'` ahead
    /// of `'standard'`; then smallest `file_size_gb` (so low-RAM boxes
    /// get a shot); Q4 quants tie-break ahead of larger quants.
    async fn rank_candidates(
        &self,
        task: &str,
        preferred: &[String],
    ) -> Result<Vec<Candidate>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, quality_tier, quantization, file_size_gb, min_vram_gb
             FROM model_catalog
             WHERE tasks @> to_jsonb(ARRAY[$1]::text[])
               AND lifecycle_status = 'active'",
        )
        .bind(task)
        .fetch_all(&self.pg)
        .await?;

        let mut candidates: Vec<Candidate> = rows
            .iter()
            .map(|r| {
                let id: String = r.get("id");
                Candidate {
                    preferred: preferred.iter().any(|p| p == &id),
                    id,
                    quality_tier: r.get("quality_tier"),
                    quantization: r.get("quantization"),
                    file_size_gb: r.get("file_size_gb"),
                    min_vram_gb: r.get("min_vram_gb"),
                }
            })
            .collect();

        candidates.sort_by(|a, b| {
            let a_pref = if a.preferred { 0 } else { 1 };
            let b_pref = if b.preferred { 0 } else { 1 };

            let a_tier = tier_rank(a.quality_tier.as_deref());
            let b_tier = tier_rank(b.quality_tier.as_deref());

            let a_q4 = if is_q4(a.quantization.as_deref()) {
                0
            } else {
                1
            };
            let b_q4 = if is_q4(b.quantization.as_deref()) {
                0
            } else {
                1
            };

            let a_size = a.file_size_gb.unwrap_or(f64::MAX);
            let b_size = b.file_size_gb.unwrap_or(f64::MAX);

            a_pref
                .cmp(&b_pref)
                .then(a_tier.cmp(&b_tier))
                .then(a_q4.cmp(&b_q4))
                .then(
                    a_size
                        .partial_cmp(&b_size)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });

        Ok(candidates)
    }

    /// Pick a computer that can host `model_id`:
    /// - online (`computers.status = 'online'`),
    /// - no active deployment of this model already,
    /// - enough RAM (`total_ram_gb >= min_vram_gb`, or `has_gpu` with sufficient VRAM).
    async fn pick_host_for(
        &self,
        model_id: &str,
        min_vram_gb: Option<f64>,
    ) -> Result<Option<String>, sqlx::Error> {
        let required = min_vram_gb.unwrap_or(0.0);

        // Only consider hosts that ALREADY have the model file in their
        // library — otherwise `ff model load <id>` fails on the chosen host
        // with `no library entry with id '<id>'`. Auto-download is a
        // separate concern (handled by hf_download / model_library_scanner).
        let row = sqlx::query(
            "SELECT c.name AS name
             FROM computers c
             WHERE c.status = 'online'
               AND EXISTS (
                   SELECT 1 FROM fleet_model_library lib
                    WHERE lib.worker_name = c.name
                      AND lib.catalog_id = $1
               )
               AND NOT EXISTS (
                   SELECT 1 FROM computer_model_deployments d
                    WHERE d.computer_id = c.id
                      AND d.model_id = $1
                      AND d.status IN ('active', 'loading')
               )
               AND (
                   (c.has_gpu AND COALESCE(c.gpu_total_vram_gb, 0) >= $2)
                   OR COALESCE(c.total_ram_gb, 0) >= $2
               )
             ORDER BY COALESCE(c.gpu_total_vram_gb, c.total_ram_gb::float, 0) DESC
             LIMIT 1",
        )
        .bind(model_id)
        .bind(required)
        .fetch_optional(&self.pg)
        .await?;

        Ok(row.map(|r| r.get("name")))
    }

    /// Enqueue a deferred shell task that invokes `ff model load <id>` on
    /// the chosen host. Runs on `node_online` so it re-fires if the box
    /// restarts before it executes.
    async fn enqueue_load(&self, model_id: &str, host_name: &str) -> Result<String, sqlx::Error> {
        let title = format!("coverage-guard auto-load {model_id} on {host_name}");
        let command = format!("ff model load {model_id}");
        let payload = serde_json::json!({ "command": command });
        let trigger_spec = serde_json::json!({ "node": host_name });

        ff_db::pg_enqueue_deferred(
            &self.pg,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(host_name),
            &serde_json::json!([]),
            Some("coverage_guard"),
            Some(3),
        )
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("pg_enqueue_deferred: {e}")))
    }
}

struct Candidate {
    id: String,
    preferred: bool,
    quality_tier: Option<String>,
    quantization: Option<String>,
    file_size_gb: Option<f64>,
    min_vram_gb: Option<f64>,
}

/// Count gaps that have at least one catalog candidate — i.e. gaps that
/// `--remediate` could enqueue an auto-load for. Used by the CLI to print a
/// discoverable hint after a read-only pass. Pure so it can be unit-tested.
pub fn loadable_gap_count(gaps: &[CoverageGap]) -> usize {
    gaps.iter().filter(|g| !g.candidates.is_empty()).count()
}

/// Extract a `Vec<String>` from a JSONB string-array column, dropping any
/// non-string elements. Returns empty for `null`/non-array values.
fn json_str_array(v: serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Credit each active deployment to the tasks it serves and return per-task
/// counts. A deployment serves a task if EITHER:
///   - its id matches ([`catalog_matches`]) an active catalog row that lists
///     the task in its `tasks`, OR
///   - its id matches a model the operator named in that task's
///     `preferred_model_ids` (`fleet_task_coverage`) — an explicit "this model
///     serves this task" declaration that overrides stale or missing catalog
///     task tags. This is how the fleet's current flagship (e.g. a deployed
///     `qwen3.6-35b-a3b`) gets credited for `default-chat`/`chain-of-thought`/
///     `code-generation` even before the catalog row's `tasks` are updated.
///
/// All ids must already be [`normalize_model_id`]-canonical. The catalog path
/// picks the single most-specific (longest) matching row. Each deployment is
/// credited at most once per task (the union of both paths), so a model that
/// matches via both catalog and preferred isn't double-counted. Pure →
/// unit-tested.
pub fn tally_task_coverage(
    deploy_norm: &[String],
    catalog: &[(String, Vec<String>)],
    preferred: &[(String, Vec<String>)],
) -> HashMap<String, i32> {
    let mut counts: HashMap<String, i32> = HashMap::new();
    for dep in deploy_norm {
        let mut tasks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        // Catalog path: the most specific matching active row contributes its
        // tasks (longest id wins so a short prefix can't shadow a precise one).
        if let Some((_, ts)) = catalog
            .iter()
            .filter(|(cid, _)| catalog_matches(dep, cid))
            .max_by_key(|(cid, _)| cid.len())
        {
            tasks.extend(ts.iter().cloned());
        }

        // Preferred path: any task whose operator-declared preferred list names
        // a model this deployment matches.
        for (task, pref_ids) in preferred {
            if pref_ids.iter().any(|p| catalog_matches(dep, p)) {
                tasks.insert(task.clone());
            }
        }

        for t in tasks {
            *counts.entry(t).or_insert(0) += 1;
        }
    }
    counts
}

/// True iff a normalized deployment id corresponds to a normalized catalog id.
/// Exact match, or the deployment id extends the catalog id at a separator
/// boundary (so `qwen-3-coder-30b-a-3b-instruct` matches catalog
/// `qwen-3-coder-30b`, but `qwen-3-72b` does NOT match `qwen-3-7b`). Both
/// inputs must already be `normalize_model_id`-canonical. Pure → unit-tested.
pub fn catalog_matches(dep_norm: &str, cat_norm: &str) -> bool {
    if cat_norm.is_empty() {
        return false;
    }
    dep_norm == cat_norm || dep_norm.starts_with(&format!("{cat_norm}-"))
}

fn tier_rank(tier: Option<&str>) -> u8 {
    match tier {
        Some("flagship") => 0,
        Some("standard") => 1,
        Some("experimental") => 2,
        _ => 3,
    }
}

fn is_q4(q: Option<&str>) -> bool {
    q.map(|s| {
        let lo = s.to_ascii_lowercase();
        lo.contains("q4") || lo.contains("4bit") || lo.contains("int4")
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ranks_flagship_first() {
        assert!(tier_rank(Some("flagship")) < tier_rank(Some("standard")));
        assert!(tier_rank(Some("standard")) < tier_rank(Some("experimental")));
        assert!(tier_rank(None) > tier_rank(Some("experimental")));
    }

    #[test]
    fn loadable_gap_count_counts_only_gaps_with_candidates() {
        let gaps = vec![
            CoverageGap {
                task: "code-generation".into(),
                min_required: 1,
                currently_loaded: 0,
                candidates: vec!["qwen3-coder-30b".into()],
            },
            CoverageGap {
                task: "default-chat".into(),
                min_required: 1,
                currently_loaded: 0,
                candidates: vec![],
            },
            CoverageGap {
                task: "image-text-to-text".into(),
                min_required: 1,
                currently_loaded: 0,
                candidates: vec!["qwen3-omni-7b".into(), "gemma3-9b".into()],
            },
        ];
        assert_eq!(loadable_gap_count(&gaps), 2);
        assert_eq!(loadable_gap_count(&[]), 0);
    }

    #[test]
    fn catalog_matches_via_normalized_forms() {
        // Real fleet case that previously returned covered=0: deployment is a
        // GGUF filename, catalog id is the compact form. Both normalize equal.
        let dep = normalize_model_id("gemma-4-31B-it-Q4_K_M.gguf");
        let cat = normalize_model_id("gemma4-31b-it");
        assert_eq!(dep, cat);
        assert!(catalog_matches(&dep, &cat));

        // Coder deployment extends the catalog id at a boundary → matches.
        let coder_dep = normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf");
        let coder_cat = normalize_model_id("qwen3-coder-30b");
        assert!(catalog_matches(&coder_dep, &coder_cat));

        // Near-miss must NOT match: 72b deployment vs 7b catalog id share a
        // textual prefix but differ at the digit, with no separator boundary.
        let d72 = normalize_model_id("qwen3-72b");
        let c7 = normalize_model_id("qwen3-7b");
        assert!(!catalog_matches(&d72, &c7));

        // A genuinely-different model (newer minor version not in catalog)
        // does not get credited.
        let newer = normalize_model_id("qwen3.6-35b-a3b");
        let older_cat = normalize_model_id("qwen35-35b-a3b");
        assert!(!catalog_matches(&newer, &older_cat));

        // Empty catalog id never matches.
        assert!(!catalog_matches("anything", ""));
    }

    #[test]
    fn tally_credits_preferred_when_catalog_tag_is_stale() {
        // The real fleet case, with the exact raw deployment ids the workers
        // record: the flagship is deployed as a dotted runtime name and as a
        // dotted GGUF filename (both fold to `qwen-3-6-35b-a-3b`). Its only
        // catalog row is a mislabeled scout candidate (image-text-to-text) —
        // filtered out here because we only pass ACTIVE catalog rows. The
        // operator declared it preferred for default-chat / chain-of-thought /
        // code-generation, so those tasks are credited via the preferred path
        // even though no active catalog row tags the model with them.
        let n = normalize_model_id;
        let deploy = vec![
            n("qwen3.6-35b-a3b"),
            n("Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"),
            n("gemma-4-31B-it-Q4_K_M.gguf"),
        ];
        // Active catalog: only gemma4 carries text-generation.
        let catalog = vec![(n("gemma4-31b-it"), vec!["text-generation".to_string()])];
        let preferred = vec![
            (
                "default-chat".to_string(),
                vec![n("qwen3.6-35b-a3b"), n("gemma4-31b-it")],
            ),
            ("chain-of-thought".to_string(), vec![n("qwen3.6-35b-a3b")]),
            ("code-generation".to_string(), vec![n("qwen3.6-35b-a3b")]),
            // No deployment matches this preferred id → not credited.
            ("code".to_string(), vec![n("qwen3-coder-30b")]),
        ];

        let counts = tally_task_coverage(&deploy, &catalog, &preferred);

        // default-chat names BOTH the flagship and gemma4 → all three
        // deployments credit it (2 flagship + 1 gemma4).
        assert_eq!(counts.get("default-chat"), Some(&3));
        // chain-of-thought / code-generation name only the flagship → its 2
        // deployments. The dotted-GGUF id prefix-matches the dotted runtime id
        // at a separator boundary, so both count.
        assert_eq!(counts.get("chain-of-thought"), Some(&2));
        assert_eq!(counts.get("code-generation"), Some(&2));
        // text-generation: only gemma4 — via catalog AND preferred — credits
        // once, not twice (union per deployment).
        assert_eq!(counts.get("text-generation"), Some(&1));
        // No deployed model matches qwen3-coder-30b → `code` stays uncovered.
        assert_eq!(counts.get("code"), None);
    }

    #[test]
    fn tally_unions_catalog_and_preferred_without_double_count() {
        let n = normalize_model_id;
        let deploy = vec![n("qwen3-coder-30b")];
        // Same model credited for `code` via BOTH catalog tag and preferred.
        let catalog = vec![(n("qwen3-coder-30b"), vec!["code".to_string()])];
        let preferred = vec![("code".to_string(), vec![n("qwen3-coder-30b")])];
        let counts = tally_task_coverage(&deploy, &catalog, &preferred);
        assert_eq!(counts.get("code"), Some(&1)); // not 2
    }

    #[test]
    fn json_str_array_filters_non_strings() {
        assert_eq!(
            json_str_array(serde_json::json!(["a", 1, "b", null])),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(json_str_array(serde_json::json!(null)).is_empty());
        assert!(json_str_array(serde_json::json!("notarray")).is_empty());
    }

    #[test]
    fn q4_detected() {
        assert!(is_q4(Some("Q4_K_M")));
        assert!(is_q4(Some("q4_0")));
        assert!(is_q4(Some("4bit")));
        assert!(!is_q4(Some("Q8_0")));
        assert!(!is_q4(None));
    }
}
