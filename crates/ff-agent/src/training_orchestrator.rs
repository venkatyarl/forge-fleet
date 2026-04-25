//! Training orchestrator — creates, starts, and tracks LoRA (and full
//! fine-tune) jobs on fleet computers.
//!
//! Lifecycle:
//!   1. `create_job`: writes a `training_jobs` row in `queued` state.
//!   2. `start_job`: enqueues a `deferred_tasks` row (kind=shell) that
//!      SSHes into the target computer and runs `scripts/train_lora_mlx.sh`
//!      (MLX on Apple Silicon) or an equivalent Linux path. The deferred
//!      worker will pick it up and execute; the training_jobs row is
//!      transitioned to `running` once the deferred task actually starts,
//!      then to `completed`/`failed` on the worker's report.
//!
//! This module does NOT implement the training loop itself — it orchestrates
//! existing repo scripts so we don't re-implement MLX/PyTorch. Loss curves
//! are fed back via `append_loss_sample` which the script can be modified
//! to call via a small HTTP/CLI callback.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum TrainingError {
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    #[error("ff-db: {0}")]
    FfDb(#[from] ff_db::DbError),
    #[error("computer '{0}' not found")]
    NotFound(String),
    #[error("base model '{0}' not found in model_catalog")]
    BaseModelNotFound(String),
    #[error("invalid job state: {0}")]
    InvalidState(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingJobSpec {
    pub name: String,
    pub base_model_id: Option<String>,
    pub training_data_path: String,
    pub adapter_output_path: Option<String>,
    pub training_type: String, // "lora" | "full_finetune" | "dpo"
    pub computer_name: Option<String>,
    pub epochs: Option<u32>,
    pub learning_rate: Option<f64>,
    pub batch_size: Option<u32>,
    pub lora_rank: Option<u32>,
    pub max_seq_len: Option<u32>,
    pub created_by: Option<String>,
}

impl TrainingJobSpec {
    /// Canonical params JSON stored on the DB row.
    fn params_json(&self) -> serde_json::Value {
        serde_json::json!({
            "epochs":        self.epochs,
            "learning_rate": self.learning_rate,
            "batch_size":    self.batch_size,
            "lora_rank":     self.lora_rank,
            "max_seq_len":   self.max_seq_len,
        })
    }
}

pub struct TrainingOrchestrator {
    pg: PgPool,
}

impl TrainingOrchestrator {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Create a new training job in `queued` state.
    pub async fn create_job(
        &self,
        spec: TrainingJobSpec,
    ) -> Result<sqlx::types::Uuid, TrainingError> {
        // Resolve computer_id if a name was supplied.
        let computer_id = if let Some(name) = spec.computer_name.as_deref() {
            Some(fetch_computer_id(&self.pg, name).await?)
        } else {
            None
        };

        // Validate base model against catalog if supplied.
        if let Some(id) = spec.base_model_id.as_deref() {
            let row = ff_db::pg_get_catalog(&self.pg, id).await?;
            if row.is_none() {
                return Err(TrainingError::BaseModelNotFound(id.into()));
            }
        }

        let params = spec.params_json();
        let id = ff_db::pg_create_training_job(
            &self.pg,
            &spec.name,
            spec.base_model_id.as_deref(),
            &spec.training_data_path,
            spec.adapter_output_path.as_deref(),
            &spec.training_type,
            computer_id,
            &params,
            spec.created_by.as_deref(),
        )
        .await?;

        info!(
            id = %id,
            name = %spec.name,
            "training job created (queued)"
        );
        Ok(id)
    }

    /// Move a queued job to running — enqueues a deferred task that runs
    /// the appropriate train_lora script on the target computer when it
    /// next comes online (or immediately, if we set trigger=now).
    pub async fn start_job(
        &self,
        id: sqlx::types::Uuid,
    ) -> Result<sqlx::types::Uuid, TrainingError> {
        let job = ff_db::pg_get_training_job(&self.pg, id)
            .await?
            .ok_or_else(|| TrainingError::InvalidState(format!("no job {id}")))?;

        if job.status != "queued" {
            return Err(TrainingError::InvalidState(format!(
                "job is in status '{}', not 'queued'",
                job.status
            )));
        }

        let computer_name = job.computer_name.clone().ok_or_else(|| {
            TrainingError::InvalidState("job has no computer_name assigned".into())
        })?;

        let command = build_training_command(&job);
        let payload = serde_json::json!({ "command": command });
        let trigger_spec = serde_json::json!({ "node": computer_name });

        let deferred_id_str = ff_db::pg_enqueue_deferred(
            &self.pg,
            &format!("training: {}", job.name),
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(&computer_name),
            &serde_json::json!([]),
            job.created_by.as_deref(),
            Some(1),
        )
        .await?;

        let deferred_uuid = sqlx::types::Uuid::parse_str(&deferred_id_str)
            .map_err(|e| TrainingError::InvalidState(format!("bad deferred uuid: {e}")))?;

        ff_db::pg_attach_training_deferred_task(&self.pg, id, deferred_uuid).await?;
        ff_db::pg_update_training_job_status(&self.pg, id, "running", None).await?;

        info!(
            id = %id,
            deferred = %deferred_uuid,
            computer = %computer_name,
            "training job dispatched"
        );
        Ok(deferred_uuid)
    }

    pub async fn status(
        &self,
        id: sqlx::types::Uuid,
    ) -> Result<ff_db::TrainingJobRow, TrainingError> {
        ff_db::pg_get_training_job(&self.pg, id)
            .await?
            .ok_or_else(|| TrainingError::InvalidState(format!("no job {id}")))
    }

    /// Called by the training worker / script to push a new loss sample
    /// onto the `loss_curve` JSONB array.
    pub async fn append_loss_sample(
        &self,
        id: sqlx::types::Uuid,
        step: i64,
        loss: f64,
    ) -> Result<(), TrainingError> {
        ff_db::pg_append_training_loss_sample(&self.pg, id, step, loss).await?;
        Ok(())
    }

    /// Terminal transition — invoked when the deferred worker reports
    /// completion or failure.
    pub async fn finish_job(
        &self,
        id: sqlx::types::Uuid,
        success: bool,
        error_message: Option<&str>,
    ) -> Result<(), TrainingError> {
        let status = if success { "completed" } else { "failed" };
        ff_db::pg_update_training_job_status(&self.pg, id, status, error_message).await?;
        if success {
            info!(id = %id, "training job completed");
        } else {
            warn!(id = %id, error = ?error_message, "training job failed");
        }
        Ok(())
    }
}

async fn fetch_computer_id(pool: &PgPool, name: &str) -> Result<sqlx::types::Uuid, TrainingError> {
    let row =
        sqlx::query_scalar::<_, sqlx::types::Uuid>("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    row.ok_or_else(|| TrainingError::NotFound(name.into()))
}

/// Build the shell command line that the deferred worker will run on the
/// target computer. For v1 we shell out to the MLX LoRA script already
/// shipped in the repo (`scripts/train_lora_mlx.sh`) and pass parameters
/// via environment variables.
fn build_training_command(job: &ff_db::TrainingJobRow) -> String {
    let mut envs = Vec::new();

    // Training data + output paths.
    envs.push(shell_env("DATASET_FILE", &job.training_data_path));
    if let Some(out) = job.adapter_output_path.as_deref() {
        envs.push(shell_env("OUTPUT_DIR", out));
    }

    // Base model — if the catalog id looks like "qwen3-coder-30b" we pass
    // it through; the script also accepts a HF repo id so it works.
    if let Some(base) = job.base_model_id.as_deref() {
        envs.push(shell_env("MODEL", base));
    }

    // Params from JSONB.
    if let Some(epochs) = job.params.get("epochs").and_then(|v| v.as_u64()) {
        envs.push(shell_env("EPOCHS", &epochs.to_string()));
    }
    if let Some(lr) = job.params.get("learning_rate").and_then(|v| v.as_f64()) {
        envs.push(shell_env("LR", &lr.to_string()));
    }
    if let Some(bs) = job.params.get("batch_size").and_then(|v| v.as_u64()) {
        envs.push(shell_env("BATCH_SIZE", &bs.to_string()));
    }
    if let Some(rank) = job.params.get("lora_rank").and_then(|v| v.as_u64()) {
        envs.push(shell_env("RANK", &rank.to_string()));
    }
    if let Some(msl) = job.params.get("max_seq_len").and_then(|v| v.as_u64()) {
        envs.push(shell_env("MAX_SEQ_LEN", &msl.to_string()));
    }

    // Also let the script know which job_id to report progress against.
    envs.push(shell_env("FORGEFLEET_TRAINING_JOB_ID", &job.id.to_string()));

    format!(
        "cd ~/taylorProjects/forge-fleet 2>/dev/null || cd ~/projects/forge-fleet; {} ./scripts/train_lora_mlx.sh",
        envs.join(" ")
    )
}

fn shell_env(key: &str, value: &str) -> String {
    let quoted = shell_quote(value);
    format!("{key}={quoted}")
}

fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}
