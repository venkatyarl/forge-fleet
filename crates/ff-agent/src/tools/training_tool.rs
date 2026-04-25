//! Training tool — lets the agent manage LoRA fine-tuning from within ForgeFleet.
//!
//! The LLM can trigger training data import, check readiness, start training,
//! and check training status — all through tool calls.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};
use crate::training::{self, ClaudeCodeImporter};

pub struct TrainingTool;

#[async_trait]
impl AgentTool for TrainingTool {
    fn name(&self) -> &str {
        "Training"
    }

    fn description(&self) -> &str {
        "Manage ForgeFleet's LoRA fine-tuning pipeline. Actions: \
         'status' — check training data readiness and adapter info, \
         'import' — import Claude Code conversations as training data, \
         'export' — export training dataset to JSONL, \
         'train' — start LoRA training on a base model using MLX, \
         'list_adapters' — list available LoRA adapters."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "import", "export", "train", "list_adapters"],
                    "description": "The training action to perform"
                },
                "model": {
                    "type": "string",
                    "description": "Base model for training (default: Qwen/Qwen3-8B). Use Qwen/Qwen3-32B for production."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status");

        match action {
            "status" => execute_status().await,
            "import" => execute_import().await,
            "export" => execute_export().await,
            "train" => {
                let model = input
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("Qwen/Qwen3-8B");
                execute_train(model, ctx).await
            }
            "list_adapters" => execute_list_adapters().await,
            _ => AgentToolResult::err(format!(
                "Unknown action: {}. Use: status, import, export, train, list_adapters",
                action
            )),
        }
    }
}

async fn execute_status() -> AgentToolResult {
    let readiness = training::readiness_check().await;
    let cc_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("projects");
    let cc_exists = cc_dir.exists();

    let mut status = format!(
        "Training Data Status:\n\
         - Examples collected: {}\n\
         - Minimum required: {}\n\
         - Recommended: {}\n\
         - Ready for training: {}\n\
         - Quality: {}\n\
         - Claude Code transcripts available: {}\n",
        readiness.example_count,
        readiness.min_required,
        readiness.recommended,
        if readiness.ready { "YES" } else { "NO" },
        readiness.quality,
        if cc_exists {
            "yes (can import with action='import')"
        } else {
            "no"
        },
    );

    // Check for existing adapters
    let adapter_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("lora_adapters");
    if adapter_dir.exists() {
        if let Ok(mut entries) = tokio::fs::read_dir(&adapter_dir).await {
            status.push_str("\nExisting adapters:\n");
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.path().is_dir() {
                    status.push_str(&format!("  - {}\n", entry.file_name().to_string_lossy()));
                }
            }
        }
    }

    // Check if training is currently running
    let log_path = std::path::Path::new("/tmp/forgefleet-lora-training.log");
    if log_path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(log_path).await {
            if content.contains("Starting training") && !content.contains("Training complete") {
                status.push_str("\n⚡ Training is currently in progress!\n");
                // Show last few lines
                let lines: Vec<&str> = content.lines().collect();
                let last = lines
                    .iter()
                    .rev()
                    .take(3)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>();
                status.push_str(&format!("Latest: {}\n", last.join(" | ")));
            }
        }
    }

    AgentToolResult::ok(status)
}

async fn execute_import() -> AgentToolResult {
    match ClaudeCodeImporter::import_all().await {
        Ok(result) => AgentToolResult::ok(format!(
            "Import complete:\n\
             - Files processed: {}\n\
             - Conversations imported: {}\n\
             - Tool calls extracted: {}\n\
             - Turns extracted: {}\n\
             - Errors: {}",
            result.files_processed,
            result.conversations_imported,
            result.tool_calls_extracted,
            result.turns_extracted,
            result.errors,
        )),
        Err(e) => AgentToolResult::err(format!("Import failed: {}", e)),
    }
}

async fn execute_export() -> AgentToolResult {
    let output = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("training_data")
        .join("dataset.jsonl");

    match training::export_dataset(&output.to_string_lossy()).await {
        Ok((path, count)) => {
            AgentToolResult::ok(format!("Exported {} examples to {}", count, path.display()))
        }
        Err(e) => AgentToolResult::err(format!("Export failed: {}", e)),
    }
}

async fn execute_train(model: &str, ctx: &AgentToolContext) -> AgentToolResult {
    let script = ctx.working_dir.join("scripts").join("train_lora_mlx.sh");
    if !script.exists() {
        // Try the default location
        let alt = dirs::home_dir()
            .unwrap_or_default()
            .join("projects")
            .join("forge-fleet")
            .join("scripts")
            .join("train_lora_mlx.sh");
        if !alt.exists() {
            return AgentToolResult::err(
                "Training script not found. Expected at scripts/train_lora_mlx.sh",
            );
        }
    }

    // Launch training in background
    let script_path = if script.exists() {
        script
    } else {
        dirs::home_dir()
            .unwrap_or_default()
            .join("projects")
            .join("forge-fleet")
            .join("scripts")
            .join("train_lora_mlx.sh")
    };

    let output = tokio::process::Command::new("bash")
        .arg(&script_path)
        .arg("--model")
        .arg(model)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match output {
        Ok(child) => {
            let pid = child.id().unwrap_or(0);
            AgentToolResult::ok(format!(
                "LoRA training started in background (PID {})!\n\
                 Model: {}\n\
                 Monitor with: tail -f /tmp/forgefleet-lora-training.log\n\
                 Use Training action='status' to check progress.",
                pid, model,
            ))
        }
        Err(e) => AgentToolResult::err(format!("Failed to start training: {}", e)),
    }
}

async fn execute_list_adapters() -> AgentToolResult {
    let adapter_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("lora_adapters");

    if !adapter_dir.exists() {
        return AgentToolResult::ok("No adapters found. Run training first.");
    }

    let mut adapters = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&adapter_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                let size = dir_size(&entry.path()).await;
                adapters.push(format!("  {} ({})", name, format_size(size)));
            }
        }
    }

    if adapters.is_empty() {
        AgentToolResult::ok("No adapters found. Run training first.")
    } else {
        AgentToolResult::ok(format!("Available LoRA adapters:\n{}", adapters.join("\n")))
    }
}

async fn dir_size(path: &std::path::Path) -> u64 {
    let mut size = 0u64;
    if let Ok(mut entries) = tokio::fs::read_dir(path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(meta) = entry.metadata().await {
                size += meta.len();
            }
        }
    }
    size
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}
