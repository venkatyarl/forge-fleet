//! Configuration and initialization for ff-LLM training jobs.
//!
//! Base-model selection is intentionally completed before initialization:
//! callers research the fleet catalog (for example with
//! `fleet_models_search`) and provide both the exact catalog id and a short
//! reference to the selection decision. This module only creates the durable
//! queued job; `ff train start <job-id>` remains the explicit execution step.

use ff_agent::training_orchestrator::{TrainingJobSpec, TrainingOrchestrator};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmTrainingInitConfig {
    pub name: String,
    pub base_model_id: String,
    pub base_model_selection: String,
    pub training_data_path: String,
    pub adapter_output_path: Option<String>,
    pub training_type: String,
    pub computer_name: String,
    pub epochs: Option<u32>,
    pub learning_rate: Option<f64>,
    pub batch_size: Option<u32>,
    pub lora_rank: Option<u32>,
    pub max_seq_len: Option<u32>,
    pub created_by: Option<String>,
}

impl LlmTrainingInitConfig {
    pub fn from_params(params: Option<Value>) -> Result<Self, String> {
        let config: Self = serde_json::from_value(
            params.ok_or_else(|| "llm_training_init requires configuration".to_string())?,
        )
        .map_err(|error| format!("invalid llm_training_init configuration: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        require_non_empty("name", &self.name)?;
        require_non_empty("base_model_id", &self.base_model_id)?;
        require_non_empty("base_model_selection", &self.base_model_selection)?;
        require_non_empty("training_data_path", &self.training_data_path)?;
        require_non_empty("computer_name", &self.computer_name)?;
        match self.training_type.as_str() {
            "lora" | "full_finetune" | "dpo" => {}
            other => {
                return Err(format!(
                    "training_type must be one of lora, full_finetune, or dpo; got '{other}'"
                ));
            }
        }
        Ok(())
    }

    fn into_spec(self) -> TrainingJobSpec {
        TrainingJobSpec {
            name: self.name,
            base_model_id: Some(self.base_model_id),
            training_data_path: self.training_data_path,
            adapter_output_path: self.adapter_output_path,
            training_type: self.training_type,
            computer_name: Some(self.computer_name),
            epochs: self.epochs,
            learning_rate: self.learning_rate,
            batch_size: self.batch_size,
            lora_rank: self.lora_rank,
            max_seq_len: self.max_seq_len,
            created_by: self.created_by,
        }
    }
}

pub async fn initialize(pool: PgPool, config: LlmTrainingInitConfig) -> Result<Value, String> {
    let base_model_id = config.base_model_id.clone();
    let base_model_selection = config.base_model_selection.clone();
    let job_id = TrainingOrchestrator::new(pool)
        .create_job(config.into_spec())
        .await
        .map_err(|error| format!("failed to initialize ff-LLM training job: {error}"))?;

    Ok(json!({
        "job_id": job_id,
        "status": "queued",
        "base_model_id": base_model_id,
        "base_model_selection": base_model_selection,
        "next_step": format!("ff train start {job_id}")
    }))
}

fn require_non_empty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_params() -> Value {
        json!({
            "name": "forge-fleet-coder-lora",
            "base_model_id": "qwen3-coder-30b",
            "base_model_selection": "Selected after fleet_models_search and benchmark review",
            "training_data_path": "/data/ff-interactions.jsonl",
            "training_type": "lora",
            "computer_name": "beyonce"
        })
    }

    #[test]
    fn config_requires_researched_base_model_selection() {
        let mut params = valid_params();
        params["base_model_selection"] = json!(" ");
        let error = LlmTrainingInitConfig::from_params(Some(params)).unwrap_err();
        assert!(error.contains("base_model_selection must not be empty"));
    }

    #[test]
    fn config_requires_explicit_catalog_id() {
        let mut params = valid_params();
        params["base_model_id"] = json!("");
        let error = LlmTrainingInitConfig::from_params(Some(params)).unwrap_err();
        assert!(error.contains("base_model_id must not be empty"));
    }

    #[test]
    fn config_rejects_unknown_training_type() {
        let mut params = valid_params();
        params["training_type"] = json!("pretraining");
        let error = LlmTrainingInitConfig::from_params(Some(params)).unwrap_err();
        assert!(error.contains("training_type must be one of"));
    }
}
