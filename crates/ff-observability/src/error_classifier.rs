//! Classification and persistence of model-server log errors.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorClass {
    StartupFailure,
    LoadError,
    Crash,
    Oom,
}

impl ModelErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StartupFailure => "startup_failure",
            Self::LoadError => "load_error",
            Self::Crash => "crash",
            Self::Oom => "oom",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelErrorEvent {
    pub error_class: ModelErrorClass,
    pub message: String,
    pub details: Value,
}

/// Identify llama.cpp's high-volume per-slot state dumps.
pub fn is_slot_dump(line: &str) -> bool {
    let line = line.trim_start().to_ascii_lowercase();
    line.starts_with("slot ")
        && (line.contains("update_slots:")
            || line.contains("load_model:")
            || line.contains("kv cache"))
}

pub fn classify(line: &str) -> Option<ModelErrorEvent> {
    if is_slot_dump(line) {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    let error_class = if contains_any(
        &lower,
        &["out of memory", "cannot allocate memory", "killed process"],
    ) || contains_word(&lower, "oom")
    {
        ModelErrorClass::Oom
    } else if contains_any(
        &lower,
        &[
            "segmentation fault",
            "core dumped",
            "panicked at",
            "fatal error",
            "terminated by signal",
        ],
    ) {
        ModelErrorClass::Crash
    } else if contains_any(
        &lower,
        &[
            "failed to load",
            "error loading model",
            "error loading weights",
            "failed to read magic",
            "invalid gguf",
        ],
    ) {
        ModelErrorClass::LoadError
    } else if contains_any(
        &lower,
        &[
            "startup failed",
            "failed to start",
            "failed to listen",
            "address already in use",
            "failed to bind",
        ],
    ) {
        ModelErrorClass::StartupFailure
    } else {
        return None;
    };

    Some(ModelErrorEvent {
        error_class,
        message: line.trim().to_owned(),
        details: serde_json::json!({"source": "model_server_log"}),
    })
}

/// Classify a log line and asynchronously persist it when it is a model-server error.
pub async fn classify_and_write(
    pool: &PgPool,
    node: &str,
    port: Option<i32>,
    model: Option<&str>,
    line: &str,
) -> Result<Option<ModelErrorEvent>, sqlx::Error> {
    let Some(event) = classify(line) else {
        return Ok(None);
    };

    sqlx::query("SELECT create_model_metrics_partitions(NOW())")
        .persistent(false)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO model_error_events (node, port, model, error_class, message, details) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .persistent(false)
    .bind(node)
    .bind(port)
    .bind(model)
    .bind(event.error_class.as_str())
    .bind(&event.message)
    .bind(&event.details)
    .execute(pool)
    .await?;

    Ok(Some(event))
}

fn contains_any(line: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| line.contains(needle))
}

fn contains_word(line: &str, word: &str) -> bool {
    line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|candidate| candidate == word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_model_server_failures() {
        assert_eq!(
            classify("CUDA error: out of memory").unwrap().error_class,
            ModelErrorClass::Oom
        );
        assert_eq!(
            classify("gguf_init: failed to read magic")
                .unwrap()
                .error_class,
            ModelErrorClass::LoadError
        );
        assert_eq!(
            classify("startup failed: address already in use")
                .unwrap()
                .error_class,
            ModelErrorClass::StartupFailure
        );
        assert_eq!(
            classify("Segmentation fault (core dumped)")
                .unwrap()
                .error_class,
            ModelErrorClass::Crash
        );
    }

    #[test]
    fn silences_slot_dumps_and_ignores_routine_logs() {
        assert!(is_slot_dump("slot update_slots: id 2 | task 4"));
        assert!(is_slot_dump("slot load_model: id 0 | new slot"));
        assert!(classify("slot update_slots: OOM counter 0").is_none());
        assert!(classify("model has room for more tokens").is_none());
        assert!(classify("server is listening on 0.0.0.0:55001").is_none());
    }
}
