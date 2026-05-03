//! Internal supervisor — ForgeFleet watches its own agent loops,
//! detects failure modes, applies fixes, and retries autonomously.
//!
//! This is the core of ForgeFleet's self-healing capability.
//! When the agent gets stuck, the supervisor diagnoses why and applies
//! targeted fixes without human intervention.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agent_loop::{AgentEvent, AgentOutcome, AgentSession, AgentSessionConfig};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Maximum retry attempts before giving up.
    pub max_attempts: u32,
    /// Base delay between retries (doubles each attempt).
    pub retry_delay_ms: u64,
    /// Flag loop if same tool+args seen this many times.
    pub loop_detection_window: usize,
    /// "Done" with fewer tool calls than this is suspicious.
    pub early_stop_min_tools: u32,
    /// Required-deliverable paths. After the agent declares done, stat each
    /// path; if any is missing or size=0, count the attempt as a failure and
    /// retry. Closes the verify-deliverable gap where agents declare "Task
    /// completed" without producing the artifact named in the prompt.
    /// See `feedback_ff_supervise_verify_deliverable.md`.
    pub verify_files: Vec<std::path::PathBuf>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            retry_delay_ms: 2000,
            loop_detection_window: 3,
            early_stop_min_tools: 1,
            verify_files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorResult {
    pub success: bool,
    pub attempts: u32,
    pub final_output: String,
    pub diagnoses: Vec<FailureDiagnosis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureDiagnosis {
    pub attempt: u32,
    pub failure_type: String,
    pub evidence: String,
    pub fix_applied: String,
}

#[derive(Debug, Clone)]
enum FailureType {
    LlmError(String),
    ToolLoop { tool: String, count: usize },
    EarlyStop { tool_count: usize },
    MaxTurnsNoProgress,
    ConsecutiveToolErrors(u32),
    NoFailure,
}

// ---------------------------------------------------------------------------
// Main supervisor function
// ---------------------------------------------------------------------------

/// Run a task with supervision — detect failures, apply fixes, retry.
pub async fn supervise(
    task: &str,
    mut agent_config: AgentSessionConfig,
    sup_config: SupervisorConfig,
) -> SupervisorResult {
    let mut diagnoses = Vec::new();
    let mut last_output = String::new();

    for attempt in 1..=sup_config.max_attempts {
        let task_preview: String = task.chars().take(100).collect();
        info!(attempt, task = %task_preview, "supervisor: starting attempt");

        // Run the agent session
        let mut session = AgentSession::new(agent_config.clone());
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

        let task_owned = task.to_string();
        let handle = tokio::spawn(async move {
            let outcome = session.run(&task_owned, Some(event_tx)).await;
            (session, outcome)
        });

        // Stream events to stderr so the operator sees live progress
        // (closes feedback_ff_supervise_no_live_progress.md gap), and
        // collect them for post-hoc failure detection.
        let mut events = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            print_event_stderr(&ev);
            events.push(ev);
        }

        let (_, outcome) = match handle.await {
            Ok(result) => result,
            Err(e) => {
                diagnoses.push(FailureDiagnosis {
                    attempt,
                    failure_type: "panic".into(),
                    evidence: format!("Agent task panicked: {e}"),
                    fix_applied: "Will retry".into(),
                });
                continue;
            }
        };

        // Extract final output
        last_output = match &outcome {
            AgentOutcome::EndTurn { final_message } => final_message.clone(),
            AgentOutcome::MaxTurns { partial_message } => partial_message.clone(),
            AgentOutcome::Error(e) => e.clone(),
            AgentOutcome::Cancelled => "Cancelled".into(),
        };

        // Detect failure
        let failure = detect_failure(&outcome, &events, &sup_config);

        match failure {
            FailureType::NoFailure => {
                // Verify declared deliverables exist before accepting success.
                // Closes the feedback_ff_supervise_verify_deliverable.md gap
                // where agents emitted "DONE" without writing the named files.
                let missing: Vec<_> = sup_config
                    .verify_files
                    .iter()
                    .filter(|p| {
                        std::fs::metadata(p)
                            .map(|m| !m.is_file() || m.len() == 0)
                            .unwrap_or(true)
                    })
                    .collect();
                if !missing.is_empty() {
                    let evidence = format!(
                        "agent declared done but {} deliverable(s) missing or empty: {}",
                        missing.len(),
                        missing
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    warn!(attempt, "{}", evidence);
                    diagnoses.push(FailureDiagnosis {
                        attempt,
                        failure_type: "missing_deliverable".into(),
                        evidence,
                        fix_applied: "Prepending stronger write-first instruction on retry".into(),
                    });
                    // Prepend a stern directive so the retry writes the files.
                    let paths_list = sup_config
                        .verify_files
                        .iter()
                        .map(|p| format!("  - {}", p.display()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let reminder = format!(
                        "CRITICAL: the previous attempt did not create the required files. \
                         You MUST invoke the Write tool (or Edit if the file exists) to create \
                         each of these files with the content described below:\n{}\n\n",
                        paths_list
                    );
                    let existing = agent_config.system_prompt.take().unwrap_or_default();
                    agent_config.system_prompt = Some(format!("{}\n{}", reminder, existing));
                    let delay = sup_config.retry_delay_ms * (1u64 << attempt);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                info!(attempt, "supervisor: task completed successfully");
                return SupervisorResult {
                    success: true,
                    attempts: attempt,
                    final_output: last_output,
                    diagnoses,
                };
            }
            ref f => {
                let diagnosis = diagnose_and_fix(f, attempt, &mut agent_config);
                warn!(
                    attempt,
                    failure = %diagnosis.failure_type,
                    fix = %diagnosis.fix_applied,
                    "supervisor: failure detected, applying fix"
                );
                diagnoses.push(diagnosis);

                // Delay before retry
                let delay = sup_config.retry_delay_ms * (1u64 << attempt);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }

    // All attempts exhausted
    warn!(
        attempts = sup_config.max_attempts,
        "supervisor: all attempts failed"
    );

    // Write failure diagnosis to Fleet Brain for learning
    let _brain_ctx = crate::brain::BrainLoader::load_for_dir(&agent_config.working_dir).await;
    let entry = crate::scoped_memory::MemoryEntry {
        id: uuid::Uuid::new_v4().to_string(),
        category: crate::scoped_memory::MemoryCategory::Learning,
        content: format!(
            "Supervisor failed after {} attempts on task: {}. Failures: {}",
            sup_config.max_attempts,
            task.chars().take(100).collect::<String>(),
            diagnoses
                .iter()
                .map(|d| d.failure_type.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        relevance: 0.8,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        source_session: None,
        tags: vec!["supervisor_failure".into()],
    };
    let brain_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("brain")
        .join("learnings.json");
    let _ = crate::learning::apply_entry(&brain_path, &entry).await;

    SupervisorResult {
        success: false,
        attempts: sup_config.max_attempts,
        final_output: last_output,
        diagnoses,
    }
}

// ---------------------------------------------------------------------------
// Live event streaming (stderr)
// ---------------------------------------------------------------------------

fn print_event_stderr(ev: &AgentEvent) {
    // ANSI dim/cyan/red — matches the existing supervisor banner style.
    const DIM: &str = "\x1b[2m";
    const CYAN: &str = "\x1b[36m";
    const RED: &str = "\x1b[31m";
    const YELLOW: &str = "\x1b[33m";
    const RESET: &str = "\x1b[0m";

    fn truncate_chars(s: &str, n: usize) -> String {
        let mut out: String = s.chars().take(n).collect();
        if s.chars().count() > n {
            out.push('…');
        }
        out
    }

    match ev {
        AgentEvent::Status { message, .. } => {
            eprintln!("    {DIM}· {message}{RESET}");
        }
        AgentEvent::AssistantText { text, .. } => {
            let preview = truncate_chars(text.trim(), 200);
            if !preview.is_empty() {
                eprintln!("    {CYAN}» {preview}{RESET}");
            }
        }
        AgentEvent::ToolStart {
            tool_name,
            input_json,
            ..
        } => {
            let preview = truncate_chars(input_json, 160);
            eprintln!("    {CYAN}⚒ {tool_name}{RESET} {DIM}{preview}{RESET}");
        }
        AgentEvent::ToolEnd {
            tool_name,
            is_error,
            duration_ms,
            result,
            ..
        } => {
            let icon = if *is_error { "✗" } else { "✓" };
            let color = if *is_error { RED } else { DIM };
            let preview = truncate_chars(result.trim(), 120);
            eprintln!(
                "    {color}{icon} {tool_name} ({duration_ms}ms){RESET} {DIM}{preview}{RESET}"
            );
        }
        AgentEvent::TurnComplete {
            turn,
            finish_reason,
            ..
        } => {
            eprintln!("    {DIM}— turn {turn} complete ({finish_reason}){RESET}");
        }
        AgentEvent::Compaction {
            messages_before,
            messages_after,
            ..
        } => {
            eprintln!(
                "    {YELLOW}⟳ compacted {messages_before} → {messages_after} messages{RESET}"
            );
        }
        AgentEvent::TokenWarning {
            usage_pct,
            estimated_tokens,
            ..
        } => {
            eprintln!(
                "    {YELLOW}⚠ context {usage_pct:.0}% full ({estimated_tokens} tokens){RESET}"
            );
        }
        AgentEvent::Error { message, .. } => {
            eprintln!("    {RED}✗ {message}{RESET}");
        }
        AgentEvent::Done { .. } => {
            eprintln!("    {DIM}✓ done{RESET}");
        }
    }
}

// ---------------------------------------------------------------------------
// Failure detection
// ---------------------------------------------------------------------------

fn detect_failure(
    outcome: &AgentOutcome,
    events: &[AgentEvent],
    config: &SupervisorConfig,
) -> FailureType {
    // 1. Explicit error
    if let AgentOutcome::Error(msg) = outcome {
        return FailureType::LlmError(msg.clone());
    }

    // Count tool calls
    let tool_ends: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolEnd { .. }))
        .collect();
    let tool_count = tool_ends.len();

    // 2. Loop detection — same tool+INPUT+result repeated.
    // Input must be in the signature because many tools have generic success
    // messages (e.g. Edit returns "Successfully edited <path> (1 replacement)"
    // for every call against the same file). Without input in the sig, N Edits
    // to N different sections of the same file would falsely trip loop detection.
    let mut tool_inputs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for ev in events {
        if let AgentEvent::ToolStart {
            tool_id, input_json, ..
        } = ev
        {
            tool_inputs.insert(tool_id.clone(), input_json.clone());
        }
    }
    let mut sig_counts = std::collections::HashMap::new();
    for ev in &tool_ends {
        if let AgentEvent::ToolEnd {
            tool_name,
            tool_id,
            result,
            ..
        } = ev
        {
            // Char-safe truncation — tool args / results often contain UTF-8.
            let input_sig: String = tool_inputs
                .get(tool_id)
                .map(|s| s.chars().take(200).collect())
                .unwrap_or_default();
            let result_sig: String = result.chars().take(50).collect();
            let sig = format!("{tool_name}:{input_sig}:{result_sig}");
            *sig_counts.entry(sig).or_insert(0usize) += 1;
        }
    }
    for (sig, count) in &sig_counts {
        if *count >= config.loop_detection_window {
            let tool = sig.split(':').next().unwrap_or("unknown").to_string();
            return FailureType::ToolLoop {
                tool,
                count: *count,
            };
        }
    }

    // 3. Early stop — completed but did almost nothing
    if matches!(outcome, AgentOutcome::EndTurn { .. })
        && tool_count < config.early_stop_min_tools as usize
    {
        if let AgentOutcome::EndTurn { final_message } = outcome {
            let lower = final_message.to_ascii_lowercase();
            if lower.contains("i'll")
                || lower.contains("i will")
                || lower.contains("i would")
                || lower.contains("i can")
                || lower.contains("let me know")
            {
                return FailureType::EarlyStop { tool_count };
            }
        }
    }

    // 4. MaxTurns with little output
    if let AgentOutcome::MaxTurns { partial_message } = outcome {
        if partial_message.len() < 100 {
            return FailureType::MaxTurnsNoProgress;
        }
    }

    // 5. High error rate
    let error_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolEnd { is_error: true, .. }))
        .count();
    if error_count > 5 && error_count as f64 / tool_count.max(1) as f64 > 0.7 {
        return FailureType::ConsecutiveToolErrors(error_count as u32);
    }

    FailureType::NoFailure
}

// ---------------------------------------------------------------------------
// Diagnosis and fix application
// ---------------------------------------------------------------------------

fn diagnose_and_fix(
    failure: &FailureType,
    attempt: u32,
    config: &mut AgentSessionConfig,
) -> FailureDiagnosis {
    match failure {
        FailureType::LlmError(msg) => FailureDiagnosis {
            attempt,
            failure_type: "llm_error".into(),
            // Char-safe truncation — `[..200]` panics on multi-byte codepoints.
            evidence: msg.chars().take(200).collect::<String>(),
            fix_applied: "Retrying with backoff (already built into agent loop)".into(),
        },
        FailureType::ToolLoop { tool, count } => {
            let fix = format!(
                "You previously called {} {} times in a row. This approach is NOT working. \
                 Try a completely different strategy. Do not repeat the same tool call.",
                tool, count
            );
            inject_system_addendum(config, &fix);
            FailureDiagnosis {
                attempt,
                failure_type: format!("tool_loop({}×{})", tool, count),
                evidence: format!("{} called {} times with similar args", tool, count),
                fix_applied: "Injected anti-loop instruction into system prompt".into(),
            }
        }
        FailureType::EarlyStop { tool_count } => {
            let fix = "You MUST actually complete the task using tools. \
                       Do NOT just describe what you would do. Use Bash, Edit, Write, or other tools \
                       to make real changes. Do not stop until the task is concretely done.";
            inject_system_addendum(config, fix);
            FailureDiagnosis {
                attempt,
                failure_type: format!("early_stop({} tools)", tool_count),
                evidence: "Agent stopped with only intent language, no tool calls".into(),
                fix_applied: "Injected tool-use enforcement".into(),
            }
        }
        FailureType::MaxTurnsNoProgress => {
            config.max_turns += 15;
            let fix = "Make concrete progress every turn. Each turn must change a file, \
                       run a command, or produce verifiable output.";
            inject_system_addendum(config, fix);
            FailureDiagnosis {
                attempt,
                failure_type: "max_turns_no_progress".into(),
                evidence: "Hit max turns with minimal output".into(),
                fix_applied: format!(
                    "Increased max_turns to {}, added progress instruction",
                    config.max_turns
                ),
            }
        }
        FailureType::ConsecutiveToolErrors(count) => {
            let fix = "Multiple tool calls are failing. Read the error messages carefully. \
                       Check file paths, permissions, and command syntax before retrying.";
            inject_system_addendum(config, fix);
            FailureDiagnosis {
                attempt,
                failure_type: format!("consecutive_errors({})", count),
                evidence: format!("{} tool calls failed", count),
                fix_applied: "Injected error-recovery guidance".into(),
            }
        }
        FailureType::NoFailure => unreachable!(),
    }
}

fn inject_system_addendum(config: &mut AgentSessionConfig, instruction: &str) {
    let current = config.system_prompt.clone().unwrap_or_default();
    config.system_prompt = Some(format!(
        "{}\n\n## IMPORTANT (Supervisor Recovery)\n{}",
        current, instruction
    ));
}
