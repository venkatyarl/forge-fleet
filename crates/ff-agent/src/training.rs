//! Training pipeline — collect data, generate fine-tuning datasets, and run LoRA training.
//!
//! Collects tool-calling examples from agent sessions and prepares them for
//! fine-tuning a LoRA adapter that makes ForgeFleet's local LLMs smarter at:
//! - Knowing when to use which tool
//! - Understanding fleet topology
//! - Multi-step task planning
//! - SSH commands, file operations, and fleet management

use std::io::BufRead;
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::fs;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Training data format
// ---------------------------------------------------------------------------

/// A single conversation turn for fine-tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingTurn {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<TrainingToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingToolCall {
    pub name: String,
    pub arguments: String,
}

/// A complete training example (multi-turn conversation with tool use).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingConversation {
    pub id: String,
    pub system_prompt: String,
    pub turns: Vec<TrainingTurn>,
    pub task_type: String,
    pub success: bool,
    pub collected_at: String,
}

// ---------------------------------------------------------------------------
// Data collection
// ---------------------------------------------------------------------------

fn training_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("training_data")
}

/// Save a training conversation.
pub async fn save_conversation(conv: &TrainingConversation) -> anyhow::Result<PathBuf> {
    let dir = training_data_dir();
    fs::create_dir_all(&dir).await?;

    let filename = format!(
        "{}_{}.json",
        Utc::now().format("%Y%m%d_%H%M%S"),
        &conv.id[..8.min(conv.id.len())]
    );
    let path = dir.join(filename);
    let json = serde_json::to_string_pretty(conv)?;
    fs::write(&path, json).await?;

    info!(path = %path.display(), turns = conv.turns.len(), "saved training conversation");
    Ok(path)
}

/// Count training examples.
pub async fn count_examples() -> usize {
    let dir = training_data_dir();
    let mut count = 0;
    if let Ok(mut entries) = fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                count += 1;
            }
        }
    }
    count
}

/// Load all training examples.
pub async fn load_all_examples() -> Vec<TrainingConversation> {
    let dir = training_data_dir();
    let mut examples = Vec::new();

    if let Ok(mut entries) = fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(entry.path()).await {
                    if let Ok(conv) = serde_json::from_str::<TrainingConversation>(&content) {
                        examples.push(conv);
                    }
                }
            }
        }
    }

    examples
}

// ---------------------------------------------------------------------------
// Dataset generation (for fine-tuning)
// ---------------------------------------------------------------------------

/// Convert training examples to the ChatML format used by most fine-tuning tools.
pub fn to_chatml_dataset(examples: &[TrainingConversation]) -> Vec<Value> {
    examples
        .iter()
        .map(|conv| {
            let mut messages = vec![json!({"role": "system", "content": conv.system_prompt})];
            for turn in &conv.turns {
                let mut msg = json!({"role": turn.role, "content": turn.content});
                if let Some(calls) = &turn.tool_calls {
                    let tc: Vec<Value> = calls
                        .iter()
                        .map(|c| {
                            json!({
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.arguments}
                            })
                        })
                        .collect();
                    msg["tool_calls"] = json!(tc);
                }
                if let Some(id) = &turn.tool_call_id {
                    msg["tool_call_id"] = json!(id);
                }
                messages.push(msg);
            }
            json!({"messages": messages})
        })
        .collect()
}

/// Export dataset to JSONL file (standard fine-tuning format).
pub async fn export_dataset(output_path: &str) -> anyhow::Result<(PathBuf, usize)> {
    let examples = load_all_examples().await;
    let dataset = to_chatml_dataset(&examples);

    let path = PathBuf::from(output_path);
    let mut content = String::new();
    for item in &dataset {
        content.push_str(&serde_json::to_string(item)?);
        content.push('\n');
    }

    fs::write(&path, &content).await?;
    info!(path = %path.display(), examples = dataset.len(), "exported training dataset");
    Ok((path, dataset.len()))
}

// ---------------------------------------------------------------------------
// LoRA fine-tuning
// ---------------------------------------------------------------------------

/// Configuration for LoRA fine-tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoraConfig {
    /// Base model path or HuggingFace ID.
    pub base_model: String,
    /// Training data JSONL path.
    pub dataset_path: String,
    /// Output directory for the LoRA adapter.
    pub output_dir: String,
    /// Number of training epochs (default: 3).
    pub epochs: u32,
    /// Learning rate (default: 2e-4).
    pub learning_rate: f64,
    /// LoRA rank (default: 16).
    pub lora_rank: u32,
    /// LoRA alpha (default: 32).
    pub lora_alpha: u32,
    /// Batch size (default: 4).
    pub batch_size: u32,
    /// Which node to train on (default: taylor).
    pub train_node: String,
    /// Training method: mlx (Mac), unsloth (CUDA), torchtune (general).
    pub method: String,
}

impl Default for LoraConfig {
    fn default() -> Self {
        Self {
            base_model: "Qwen/Qwen2.5-Coder-32B-Instruct".into(),
            dataset_path: "~/.forgefleet/training_data/dataset.jsonl".into(),
            output_dir: "~/.forgefleet/lora_adapters/forgefleet-v1".into(),
            epochs: 3,
            learning_rate: 2e-4,
            lora_rank: 16,
            lora_alpha: 32,
            batch_size: 4,
            train_node: "taylor".into(),
            method: "mlx".into(),
        }
    }
}

/// Generate the training command based on config.
pub fn generate_training_command(config: &LoraConfig) -> String {
    match config.method.as_str() {
        "mlx" => {
            // MLX LoRA (Apple Silicon — for Taylor Mac Studio)
            format!(
                "python3 -m mlx_lm.lora \\\n\
                 --model {base_model} \\\n\
                 --data {dataset} \\\n\
                 --adapter-path {output} \\\n\
                 --train \\\n\
                 --iters {iters} \\\n\
                 --learning-rate {lr} \\\n\
                 --lora-layers {rank} \\\n\
                 --batch-size {batch}",
                base_model = config.base_model,
                dataset = config.dataset_path,
                output = config.output_dir,
                iters = config.epochs * 100, // approximate
                lr = config.learning_rate,
                rank = config.lora_rank,
                batch = config.batch_size,
            )
        }
        "unsloth" => {
            // Unsloth (NVIDIA CUDA — for DGX Sparks)
            format!(
                "python3 -c \"\n\
                from unsloth import FastLanguageModel\n\
                model, tokenizer = FastLanguageModel.from_pretrained('{base_model}', max_seq_length=4096, load_in_4bit=True)\n\
                model = FastLanguageModel.get_peft_model(model, r={rank}, lora_alpha={alpha})\n\
                # Training with SFTTrainer...\n\
                from trl import SFTTrainer\n\
                trainer = SFTTrainer(model=model, train_dataset='{dataset}', max_seq_length=4096)\n\
                trainer.train()\n\
                model.save_pretrained('{output}')\n\
                \"",
                base_model = config.base_model,
                dataset = config.dataset_path,
                output = config.output_dir,
                rank = config.lora_rank,
                alpha = config.lora_alpha,
            )
        }
        _ => {
            format!(
                "# Unsupported method: {}. Use 'mlx' or 'unsloth'.",
                config.method
            )
        }
    }
}

/// Check if we have enough training data for fine-tuning.
pub async fn readiness_check() -> TrainingReadiness {
    let count = count_examples().await;
    let min_examples = 100;
    let recommended = 500;

    TrainingReadiness {
        example_count: count,
        min_required: min_examples,
        recommended,
        ready: count >= min_examples,
        quality: if count >= recommended {
            "good"
        } else if count >= min_examples {
            "minimum"
        } else {
            "insufficient"
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingReadiness {
    pub example_count: usize,
    pub min_required: usize,
    pub recommended: usize,
    pub ready: bool,
    pub quality: &'static str,
}

// ---------------------------------------------------------------------------
// Prompt engineering system
// ---------------------------------------------------------------------------

/// Optimized prompts for different task types.
/// These are refined through usage and can be updated without recompiling.
pub struct PromptLibrary;

impl PromptLibrary {
    /// Get the optimized prompt for a task type.
    pub fn get_prompt(task_type: &str) -> String {
        match task_type {
            "ssh" | "fleet_op" => r#"You are ForgeFleet, managing a fleet of computers. When asked to SSH or interact with fleet nodes, ALWAYS use the Bash tool with SSH commands. Never say you can't SSH — you have access.

Fleet nodes:
- Taylor (192.168.5.100) user:venkat — leader, Mac Studio 96GB
- Marcus (192.168.5.102) user:marcus — Ubuntu 32GB, Qwen2.5-Coder-32B
- Sophie (192.168.5.103) user:sophie — Ubuntu 32GB, Qwen2.5-Coder-32B
- Priya  (192.168.5.104) user:priya  — Ubuntu 32GB, Qwen2.5-Coder-32B
- James  (192.168.5.108) user:james  — Mac mini 64GB, Qwen2.5-72B

To SSH: ssh user@ip 'command here'
Always include a command — never open interactive SSH."#.into(),

            "coding" => r#"You are ForgeFleet, a coding agent. When asked to write or modify code:
1. Read the existing code first (Read tool)
2. Make changes (Edit tool for existing files, Write for new files)
3. Verify changes (Read the modified file or run tests with Bash)
Be precise with Edit — old_string must match exactly."#.into(),

            "research" => r#"You are ForgeFleet, a research agent. When asked to research:
1. Use WebSearch to find relevant information
2. Use WebFetch to read detailed pages
3. Summarize findings clearly with sources"#.into(),

            "review" => r#"You are ForgeFleet, a code reviewer. When asked to review:
1. Read the changed files (Read + Grep)
2. Check for bugs, security issues, style problems
3. Provide specific, actionable feedback"#.into(),

            _ => r#"You are ForgeFleet, an AI agent with access to tools. Use tools when needed to accomplish tasks. Be concise and action-oriented."#.into(),
        }
    }

    /// Get all available prompt types.
    pub fn available_types() -> Vec<&'static str> {
        vec!["ssh", "fleet_op", "coding", "research", "review", "general"]
    }
}

// ---------------------------------------------------------------------------
// Claude Code transcript importer
// ---------------------------------------------------------------------------

/// Import training data from Claude Code JSONL transcripts.
///
/// Claude Code stores conversation transcripts as JSONL files where each line
/// is a message event. The format is:
///   - type: "user" | "assistant" | "system" | "attachment" | ...
///   - message: { role, content } — for user messages, content is a string or
///     array of content blocks (tool_result, text)
///   - message: { role, content, ... } — for assistant messages, content is an
///     array of blocks (text, thinking, tool_use)
///
/// This converter extracts tool-calling patterns and converts them into
/// ForgeFleet's TrainingConversation format for LoRA fine-tuning.
pub struct ClaudeCodeImporter;

impl ClaudeCodeImporter {
    /// Import all Claude Code transcripts from the default location.
    pub async fn import_all() -> anyhow::Result<ImportResult> {
        let cc_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".claude")
            .join("projects");

        let mut total = ImportResult::default();

        if !cc_dir.exists() {
            info!(
                "no Claude Code projects directory found at {}",
                cc_dir.display()
            );
            return Ok(total);
        }

        // Walk project directories looking for .jsonl files
        let mut stack = vec![cc_dir];
        while let Some(dir) = stack.pop() {
            if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        match Self::import_transcript(&path).await {
                            Ok(result) => {
                                total.files_processed += 1;
                                total.conversations_imported += result.conversations_imported;
                                total.tool_calls_extracted += result.tool_calls_extracted;
                                total.turns_extracted += result.turns_extracted;
                            }
                            Err(e) => {
                                warn!(path = %path.display(), error = %e, "failed to import transcript");
                                total.errors += 1;
                            }
                        }
                    }
                }
            }
        }

        info!(
            files = total.files_processed,
            conversations = total.conversations_imported,
            tool_calls = total.tool_calls_extracted,
            "Claude Code import complete"
        );

        Ok(total)
    }

    /// Import a single Claude Code JSONL transcript file.
    pub async fn import_transcript(path: &std::path::Path) -> anyhow::Result<ImportResult> {
        let content = tokio::fs::read(path).await?;
        let reader = std::io::BufReader::new(content.as_slice());

        let mut entries: Vec<Value> = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<Value>(&line) {
                entries.push(val);
            }
        }

        let session_id = entries
            .iter()
            .find_map(|e| e.get("sessionId").and_then(Value::as_str))
            .unwrap_or("unknown")
            .to_string();

        // Extract system prompt
        let system_prompt = entries
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("system"))
            .find_map(|e| {
                let msg = e.get("message")?;
                let content = msg.get("content")?;
                if let Value::String(s) = content {
                    Some(s.clone())
                } else if let Value::Array(arr) = content {
                    arr.iter().find_map(|b| {
                        if b.get("type").and_then(Value::as_str) == Some("text") {
                            b.get("text").and_then(Value::as_str).map(String::from)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "You are an AI coding assistant.".into());

        // Build conversation turns from user/assistant messages
        let mut turns = Vec::new();
        let mut tool_call_count = 0usize;

        for entry in &entries {
            let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
            let msg = match entry.get("message") {
                Some(m) => m,
                None => continue,
            };

            match entry_type {
                "user" => {
                    let content = msg.get("content");
                    match content {
                        Some(Value::String(s)) => {
                            turns.push(TrainingTurn {
                                role: "user".into(),
                                content: s.clone(),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                        Some(Value::Array(blocks)) => {
                            // Could be text + tool_result blocks
                            for block in blocks {
                                let block_type =
                                    block.get("type").and_then(Value::as_str).unwrap_or("");
                                match block_type {
                                    "text" => {
                                        if let Some(text) =
                                            block.get("text").and_then(Value::as_str)
                                        {
                                            if !text.is_empty() {
                                                turns.push(TrainingTurn {
                                                    role: "user".into(),
                                                    content: text.to_string(),
                                                    tool_calls: None,
                                                    tool_call_id: None,
                                                });
                                            }
                                        }
                                    }
                                    "tool_result" => {
                                        let tool_use_id = block
                                            .get("tool_use_id")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string();
                                        let result_content = block
                                            .get("content")
                                            .map(|c| match c {
                                                Value::String(s) => truncate_for_training(s, 2000),
                                                other => {
                                                    truncate_for_training(&other.to_string(), 2000)
                                                }
                                            })
                                            .unwrap_or_default();

                                        turns.push(TrainingTurn {
                                            role: "tool".into(),
                                            content: result_content,
                                            tool_calls: None,
                                            tool_call_id: Some(tool_use_id),
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }

                "assistant" => {
                    let content = match msg.get("content") {
                        Some(Value::Array(arr)) => arr.clone(),
                        _ => continue,
                    };

                    let mut text_parts = Vec::new();
                    let mut tool_calls = Vec::new();

                    for block in &content {
                        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        text_parts.push(text.to_string());
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown")
                                    .to_string();
                                let input = block
                                    .get("input")
                                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                                    .unwrap_or_default();
                                tool_calls.push(TrainingToolCall {
                                    name,
                                    arguments: truncate_for_training(&input, 2000),
                                });
                                tool_call_count += 1;
                            }
                            // Skip "thinking" blocks — internal reasoning
                            _ => {}
                        }
                    }

                    let combined_text = text_parts.join("\n");
                    turns.push(TrainingTurn {
                        role: "assistant".into(),
                        content: combined_text,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                    });
                }

                _ => {} // skip attachment, file-history-snapshot, etc.
            }
        }

        if turns.is_empty() {
            return Ok(ImportResult::default());
        }

        // Split into conversation chunks (by user messages after a gap)
        // For now, treat the entire transcript as one conversation
        let conv = TrainingConversation {
            id: session_id.clone(),
            system_prompt: truncate_for_training(&system_prompt, 1000),
            turns,
            task_type: "coding".into(), // CC transcripts are primarily coding
            success: true,
            collected_at: Utc::now().to_rfc3339(),
        };

        let turns_count = conv.turns.len();
        save_conversation(&conv).await?;

        Ok(ImportResult {
            files_processed: 1,
            conversations_imported: 1,
            tool_calls_extracted: tool_call_count,
            turns_extracted: turns_count,
            errors: 0,
        })
    }
}

fn truncate_for_training(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...[truncated]", &s[..max])
    }
}

/// Result of importing Claude Code transcripts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportResult {
    pub files_processed: usize,
    pub conversations_imported: usize,
    pub tool_calls_extracted: usize,
    pub turns_extracted: usize,
    pub errors: usize,
}
