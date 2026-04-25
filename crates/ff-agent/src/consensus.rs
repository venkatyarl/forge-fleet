//! Consensus coding — N-of-M agent agreement for critical code changes.
//!
//! Dispatches the same task to N agents on different fleet nodes (or same node
//! with different temperatures), collects solutions, runs tests, picks the best.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::agent_loop::{AgentOutcome, AgentSession, AgentSessionConfig};

/// Configuration for consensus coding.
#[derive(Debug, Clone)]
pub struct ConsensusConfig {
    /// Number of agents to run in parallel.
    pub agent_count: usize,
    /// Fleet endpoints to use (one per agent).
    pub endpoints: Vec<String>,
    /// Working directory.
    pub working_dir: PathBuf,
    /// Test command to validate solutions (e.g., "cargo test").
    pub test_command: Option<String>,
    /// Max turns per agent.
    pub max_turns: u32,
}

/// Result of consensus coding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusResult {
    pub solutions: Vec<SolutionResult>,
    pub winner: Option<usize>,
    pub winner_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionResult {
    pub agent_index: usize,
    pub endpoint: String,
    pub output: String,
    pub tests_passed: Option<bool>,
    pub duration_ms: u64,
    pub status: String,
}

/// Run consensus coding: N agents solve the same problem, pick the best.
pub async fn run_consensus(prompt: &str, config: &ConsensusConfig) -> ConsensusResult {
    info!(agents = config.agent_count, "starting consensus coding");

    let mut handles = Vec::new();

    for (i, endpoint) in config.endpoints.iter().take(config.agent_count).enumerate() {
        let prompt = prompt.to_string();
        let endpoint = endpoint.clone();
        let working_dir = config.working_dir.clone();
        let max_turns = config.max_turns;

        handles.push(tokio::spawn(async move {
            let start = std::time::Instant::now();

            let agent_config = AgentSessionConfig {
                model: "auto".into(),
                llm_base_url: endpoint.clone(),
                working_dir,
                system_prompt: None,
                max_turns,
                auto_save: false,
                ..Default::default()
            };

            let mut session = AgentSession::new(agent_config);
            let outcome = session.run(&prompt, None).await;
            let duration_ms = start.elapsed().as_millis() as u64;

            let (output, status) = match outcome {
                AgentOutcome::EndTurn { final_message } => (final_message, "completed"),
                AgentOutcome::MaxTurns { partial_message } => (partial_message, "max_turns"),
                AgentOutcome::Error(e) => (e, "error"),
                AgentOutcome::Cancelled => ("cancelled".into(), "cancelled"),
            };

            SolutionResult {
                agent_index: i,
                endpoint,
                output,
                tests_passed: None,
                duration_ms,
                status: status.into(),
            }
        }));
    }

    let mut solutions = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(result) => solutions.push(result),
            Err(e) => solutions.push(SolutionResult {
                agent_index: solutions.len(),
                endpoint: "unknown".into(),
                output: format!("Panic: {e}"),
                tests_passed: None,
                duration_ms: 0,
                status: "error".into(),
            }),
        }
    }

    // Run tests on each solution if test command is provided
    if let Some(test_cmd) = &config.test_command {
        for solution in &mut solutions {
            if solution.status == "completed" {
                let test_output = tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(test_cmd)
                    .current_dir(&config.working_dir)
                    .output()
                    .await;

                solution.tests_passed =
                    Some(test_output.map(|o| o.status.success()).unwrap_or(false));
            }
        }
    }

    // Pick winner: first passing solution, or first completed
    let winner = solutions
        .iter()
        .position(|s| s.tests_passed == Some(true))
        .or_else(|| solutions.iter().position(|s| s.status == "completed"));

    let winner_reason = match winner {
        Some(i) if solutions[i].tests_passed == Some(true) => {
            format!(
                "Agent {} — tests passed in {}ms",
                i, solutions[i].duration_ms
            )
        }
        Some(i) => {
            format!(
                "Agent {} — completed first in {}ms (no test validation)",
                i, solutions[i].duration_ms
            )
        }
        None => "No agent completed successfully".into(),
    };

    info!(winner = ?winner, reason = %winner_reason, "consensus coding complete");

    ConsensusResult {
        solutions,
        winner,
        winner_reason,
    }
}
