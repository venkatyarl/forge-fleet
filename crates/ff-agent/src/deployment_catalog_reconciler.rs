//! Deployment → catalog reconciler (coverage self-heal).
//!
//! The fleet coverage guard ([`crate::coverage_guard`]) credits a task only
//! when a deployed model matches an **active** `model_catalog` row tagged with
//! that task (or an operator-declared `preferred_model_ids` entry). When a
//! model is downloaded + deployed directly — without ever passing through the
//! scout's candidate pipeline or a manual `catalog-add` — it has *no*
//! `model_catalog` row at all, so coverage reports a false gap even though the
//! fleet is demonstrably serving the task. The real fleet hit this for
//! `bge-m3` (an embedding model serving `feature-extraction`) and
//! `qwen3-vl-30b-a3b` (a vision model serving `image-text-to-text`): both were
//! live yet coverage showed those tasks uncovered.
//!
//! This module closes that declaration gap **conservatively**. For each active
//! deployment whose id matches no existing catalog row, it auto-creates an
//! `active` catalog row — but ONLY when the model family is *structurally
//! unambiguous* (an embedding model IS feature-extraction; a `-vl-`/vision
//! model IS image-text-to-text; a whisper model IS ASR; a reranker IS
//! text-ranking). Ambiguous general chat/code models are deliberately LEFT
//! ALONE: their task set is a judgement call the operator owns, and
//! auto-tagging them is exactly the mislabeling that made the scout's raw
//! `pipeline_tag` candidates untrustworthy (a text MoE scouted as
//! image-text-to-text). See [`classify_deployment_tasks`].
//!
//! Writes use `ON CONFLICT (id) DO NOTHING`, so an operator-curated row always
//! wins — the reconciler only ever *adds* a row that was entirely missing, it
//! never overwrites task tags or lifecycle a human set.
//!
//! Runs leader-gated on a slow cadence from `forgefleetd` (see
//! `portfolio_maintenance`) and is exposed manually as `ff model
//! reconcile-catalog` for dogfooding.

use std::collections::HashSet;

use ff_core::model_id::normalize_model_id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{debug, info};

use crate::coverage_guard::catalog_matches;

/// One catalog row the reconciler created (or would create, in dry-run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciledRow {
    /// Clean catalog id minted for the deployment.
    pub catalog_id: String,
    /// Raw deployment `model_id` the row was derived from.
    pub from_deployment: String,
    /// Tasks tagged on the new row.
    pub tasks: Vec<String>,
}

/// Result of one reconcile pass.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ReconcileReport {
    /// Rows created (or, in dry-run, that would be created).
    pub created: Vec<ReconciledRow>,
    /// Distinct deployments whose family is ambiguous (general chat/code) and
    /// were left for the operator to declare. Reported for visibility.
    pub skipped_ambiguous: Vec<String>,
    /// Deployments already covered by an existing catalog row (no action).
    pub already_cataloged: usize,
    /// True if the pass made no DB writes.
    pub dry_run: bool,
}

/// Reconciles active deployments into `model_catalog` rows. See module docs.
#[derive(Clone)]
pub struct DeploymentCatalogReconciler {
    pg: PgPool,
}

impl DeploymentCatalogReconciler {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Run one pass. When `dry_run` is true, classify + report but write
    /// nothing. Returns the rows created and the ambiguous deployments skipped.
    pub async fn reconcile_once(&self, dry_run: bool) -> Result<ReconcileReport, sqlx::Error> {
        // Distinct model ids of currently-active deployments.
        let dep_rows = sqlx::query(
            "SELECT DISTINCT model_id FROM computer_model_deployments WHERE status = 'active'",
        )
        .fetch_all(&self.pg)
        .await?;

        // Every existing catalog id (any lifecycle), normalized — used to skip
        // deployments that already have a row (active, candidate, or deprecated).
        let cat_rows = sqlx::query("SELECT id FROM model_catalog")
            .fetch_all(&self.pg)
            .await?;
        let cat_norm: Vec<String> = cat_rows
            .iter()
            .map(|r| normalize_model_id(&r.get::<String, _>("id")))
            .collect();

        let mut report = ReconcileReport {
            dry_run,
            ..Default::default()
        };
        // Dedup multiple deployments (e.g. several GGUF quants) that collapse to
        // the same minted catalog id within a single pass.
        let mut minted: HashSet<String> = HashSet::new();

        for row in &dep_rows {
            let raw: String = row.get("model_id");
            let dep_norm = normalize_model_id(&raw);

            // Already represented in the catalog? Nothing to do.
            if cat_norm.iter().any(|c| catalog_matches(&dep_norm, c)) {
                report.already_cataloged += 1;
                continue;
            }

            let catalog_id = derive_catalog_id(&raw);
            match classify_deployment_tasks(&catalog_id) {
                Some(tasks) => {
                    if !minted.insert(catalog_id.clone()) {
                        // Already minted this pass from a sibling quant.
                        continue;
                    }
                    if !dry_run {
                        self.insert_active_row(&catalog_id, &tasks).await?;
                    }
                    info!(
                        catalog_id = %catalog_id,
                        from = %raw,
                        tasks = ?tasks,
                        dry_run,
                        "deployment-catalog reconciler: minted catalog row"
                    );
                    report.created.push(ReconciledRow {
                        catalog_id,
                        from_deployment: raw,
                        tasks,
                    });
                }
                None => {
                    debug!(
                        from = %raw,
                        "deployment-catalog reconciler: ambiguous family, left for operator"
                    );
                    report.skipped_ambiguous.push(raw);
                }
            }
        }

        info!(
            created = report.created.len(),
            skipped_ambiguous = report.skipped_ambiguous.len(),
            already_cataloged = report.already_cataloged,
            dry_run,
            "deployment-catalog reconcile pass complete"
        );
        Ok(report)
    }

    /// Insert an `active` catalog row for a structurally-classified deployment.
    /// `ON CONFLICT (id) DO NOTHING` so an operator-curated row always wins.
    async fn insert_active_row(&self, id: &str, tasks: &[String]) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO model_catalog
                 (id, display_name, family, tasks, lifecycle_status, added_by, notes)
             VALUES ($1, $2, $3, $4, 'active', 'deployment-reconciler',
                     'auto-declared from a live deployment by the coverage self-heal')
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(id) // display_name = id; operator can rename
        .bind(derive_family(id))
        .bind(json!(tasks))
        .execute(&self.pg)
        .await?;
        Ok(())
    }
}

/// Classify a deployment id into the HF tasks it *structurally* serves, or
/// `None` when the family is ambiguous (general chat/code) and should be left
/// for the operator. Only families whose task is determined by architecture —
/// not by judgement — are returned.
///
/// Detection runs on the [`normalize_model_id`] form, NOT the raw string: the
/// same model surfaces under many spellings (`Qwen3VL-30B-A3B-Instruct-Q4_K_M`,
/// `qwen3-vl-30b`, a GGUF filename), and only the normalized form reliably
/// flanks a family token with dashes — it lowercases, folds dots to dashes,
/// inserts a separator at every digit/letter boundary (`Qwen3VL` →
/// `qwen-3-vl`), and strips quant/format suffixes. Without this the
/// no-hyphen `Qwen3VL` deployment was misclassified as ambiguous (caught in
/// dogfooding). Order matters: reranker is checked before embedding because
/// `bge-reranker-*` shares the `bge` prefix with embedding models. Pure →
/// unit-tested.
pub fn classify_deployment_tasks(id_in: &str) -> Option<Vec<String>> {
    let id = normalize_model_id(id_in);
    let has = |needle: &str| id.contains(needle);

    // Speech-to-text.
    if has("whisper") || has("-asr") {
        return Some(vec!["automatic-speech-recognition".into()]);
    }
    // Rerankers (check before embedding: `bge-reranker-*` contains `bge`).
    if has("reranker") || has("-rerank") {
        return Some(vec!["text-ranking".into()]);
    }
    // Embedding / feature-extraction families. Prefix checks use the
    // digit-split normalized form (`e5` → `e-5`).
    if has("embed")
        || id.starts_with("bge-")
        || id.starts_with("gte-")
        || id.starts_with("e-5-")
        || has("-e-5-")
        || has("nomic-embed")
        || has("sentence")
        || has("minilm")
    {
        return Some(vec!["feature-extraction".into()]);
    }
    // Vision-language families. `normalize_model_id` splits only on
    // letter→digit boundaries, so `Qwen3VL` becomes `qwen-3vl` (NOT
    // `qwen-3-vl`) — the `vl` token rides on the preceding digit. Match it
    // per dash-segment: a segment that is `vl`/`vlm`, or ends in `vl`/`vlm`
    // with only digits before it (`3vl`, `2vlm`).
    let is_vl_segment = |s: &str| {
        let core = s.strip_suffix("vlm").or_else(|| s.strip_suffix("vl"));
        matches!(core, Some(prefix) if prefix.chars().all(|c| c.is_ascii_digit()))
    };
    if id.split('-').any(is_vl_segment)
        || has("vision")
        || has("llava")
        || has("-omni")
        || has("internvl")
    {
        return Some(vec!["image-text-to-text".into(), "text-generation".into()]);
    }
    // Anything else (chat, code, MoE) — operator's judgement.
    None
}

/// Derive a clean catalog id from a deployment `model_id`. Lowercases, strips a
/// trailing `.gguf`, and pops trailing quant/format tokens (`-Q4_K_M`, `-UD`,
/// `-f16`, …) so several quants of one model collapse to a single catalog id.
/// Pure → unit-tested.
pub fn derive_catalog_id(model_id: &str) -> String {
    let mut s = model_id.trim().to_ascii_lowercase();
    if let Some(stripped) = s.strip_suffix(".gguf") {
        s = stripped.to_string();
    }
    // Drop org/ prefix if present (e.g. `qwen/qwen3-vl-30b`).
    if let Some((_, name)) = s.rsplit_once('/') {
        s = name.to_string();
    }
    let mut parts: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
    while parts.len() > 1 {
        if is_quant_token(parts[parts.len() - 1]) {
            parts.pop();
        } else {
            break;
        }
    }
    parts.join("-")
}

/// True for trailing tokens that encode a quant/format, not a model identity.
fn is_quant_token(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    // qN / qN_x quants (q4, q5, q8, q4_k_m all arrive as one dash-segment).
    if t.strip_prefix('q')
        .and_then(|r| r.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
    {
        return true;
    }
    matches!(
        t.as_str(),
        "k_m"
            | "k_s"
            | "k_l"
            | "f16"
            | "bf16"
            | "fp16"
            | "f32"
            | "fp32"
            | "ud"
            | "mlx"
            | "awq"
            | "gptq"
            | "gguf"
            | "int4"
            | "int8"
            | "4bit"
            | "8bit"
    )
}

/// Best-effort family from a clean id: the leading dash-segment.
fn derive_family(id: &str) -> String {
    id.split('-').next().unwrap_or(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_embedding_families() {
        assert_eq!(
            classify_deployment_tasks("bge-m3"),
            Some(vec!["feature-extraction".to_string()])
        );
        assert_eq!(
            classify_deployment_tasks("qwen3-embedding-8b"),
            Some(vec!["feature-extraction".to_string()])
        );
        assert_eq!(
            classify_deployment_tasks("nomic-embed-text-v1.5"),
            Some(vec!["feature-extraction".to_string()])
        );
    }

    #[test]
    fn rerankers_classify_before_embedding() {
        // Shares the `bge` prefix with embedding models but must be ranking.
        assert_eq!(
            classify_deployment_tasks("bge-reranker-v2-m3"),
            Some(vec!["text-ranking".to_string()])
        );
    }

    #[test]
    fn classifies_vision_families() {
        assert_eq!(
            classify_deployment_tasks("qwen3-vl-30b-a3b"),
            Some(vec![
                "image-text-to-text".to_string(),
                "text-generation".to_string()
            ])
        );
        assert!(classify_deployment_tasks("internvl2-8b").is_some());
        assert!(classify_deployment_tasks("llava-1.6-7b").is_some());
    }

    #[test]
    fn classifies_asr() {
        assert_eq!(
            classify_deployment_tasks("whisper-large-v3"),
            Some(vec!["automatic-speech-recognition".to_string()])
        );
    }

    #[test]
    fn classifies_real_raw_deployment_ids() {
        // Regression: the exact raw `computer_model_deployments.model_id`
        // strings seen on the live fleet (GGUF filenames, the no-hyphen
        // `Qwen3VL` spelling, a reranker sharing the `bge` prefix). The
        // pipeline is derive_catalog_id → classify; assert the end result.
        let cases = [
            (
                "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf",
                Some(vec![
                    "image-text-to-text".to_string(),
                    "text-generation".to_string(),
                ]),
            ),
            ("bge-m3-FP16.gguf", Some(vec!["feature-extraction".into()])),
            (
                "bge-reranker-v2-m3-FP16.gguf",
                Some(vec!["text-ranking".into()]),
            ),
            ("minimax-m2.7", None),
            ("qwen3-next-80b-a3b", None),
            ("/Users/venkat/models/qwen36-35b-a3b", None),
        ];
        for (raw, want) in cases {
            let id = derive_catalog_id(raw);
            assert_eq!(classify_deployment_tasks(&id), want, "raw={raw} id={id}");
        }
    }

    #[test]
    fn leaves_ambiguous_chat_code_models_alone() {
        // The exact false-coverage trap: general chat/code/MoE models have no
        // structural task and must NOT be auto-tagged.
        assert_eq!(classify_deployment_tasks("qwen36-35b-a3b"), None);
        assert_eq!(classify_deployment_tasks("minimax-m27"), None);
        assert_eq!(classify_deployment_tasks("qwen3-next-80b-a3b"), None);
        assert_eq!(classify_deployment_tasks("gemma4-31b-it"), None);
        assert_eq!(classify_deployment_tasks("qwen3-coder-30b"), None);
    }

    #[test]
    fn derive_catalog_id_strips_quant_and_gguf() {
        assert_eq!(derive_catalog_id("bge-m3"), "bge-m3");
        assert_eq!(derive_catalog_id("qwen3-vl-30b-a3b"), "qwen3-vl-30b-a3b");
        assert_eq!(
            derive_catalog_id("Qwen3-VL-30B-A3B-Q4_K_M.gguf"),
            "qwen3-vl-30b-a3b"
        );
        assert_eq!(
            derive_catalog_id("Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"),
            "qwen3.6-35b-a3b"
        );
        assert_eq!(derive_catalog_id("Qwen/Qwen3-Coder-30B"), "qwen3-coder-30b");
    }

    #[test]
    fn derive_catalog_id_keeps_model_identity_tokens() {
        // `m3`, `a3b` are identity, not quant — must survive.
        assert_eq!(derive_catalog_id("bge-m3-Q8_0.gguf"), "bge-m3");
        assert_eq!(
            derive_catalog_id("qwen3-vl-30b-a3b-f16"),
            "qwen3-vl-30b-a3b"
        );
    }

    #[test]
    fn derive_family_takes_leading_segment() {
        assert_eq!(derive_family("bge-m3"), "bge");
        assert_eq!(derive_family("qwen3-vl-30b-a3b"), "qwen3");
    }
}
