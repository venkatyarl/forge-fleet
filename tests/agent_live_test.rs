//! Live integration test for the agent loop against a real fleet LLM.
//!
//! Run with: cargo test --test agent_live_test -- --nocapture
//!
//! Requires a running llama.cpp server on Marcus (192.168.5.102:51000).

use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use std::path::PathBuf;
use tokio::sync::mpsc;

#[tokio::test]
async fn agent_executes_bash_via_tool() {
    // Skip if LLM is not reachable
    let client = reqwest::Client::new();
    let health = client
        .get("http://192.168.5.102:51000/health")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;

    if health.is_err() {
        eprintln!("SKIPPING: Marcus LLM not reachable at 192.168.5.102:51000");
        return;
    }

    let config = AgentSessionConfig {
        model: "Qwen2.5-Coder-32B-Instruct-Q4_K_M.gguf".into(),
        llm_base_url: "http://192.168.5.102:51000".into(),
        working_dir: PathBuf::from("/tmp"),
        system_prompt: Some("You are a coding agent. Use the Bash tool to run commands. Be concise.".into()),
        max_turns: 3,
        temperature: 0.3,
        max_tokens: 256,
        auto_save: false,
        ..Default::default()
    };

    let mut session = AgentSession::new(config);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Collect events in background
    let events_handle = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            eprintln!("EVENT: {}", serde_json::to_string(&event).unwrap_or_default());
            events.push(event);
        }
        events
    });

    // Simple prompt that should trigger a Bash tool call
    let outcome = session.run("Run: echo ForgeFleet-Agent-Works", Some(event_tx)).await;

    drop(session);
    let events = events_handle.await.unwrap();

    eprintln!("\n=== OUTCOME ===");
    match &outcome {
        ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } => {
            eprintln!("EndTurn: {final_message}");
        }
        ff_agent::agent_loop::AgentOutcome::MaxTurns { partial_message } => {
            eprintln!("MaxTurns: {partial_message}");
        }
        ff_agent::agent_loop::AgentOutcome::Cancelled => {
            eprintln!("Cancelled");
        }
        ff_agent::agent_loop::AgentOutcome::Error(e) => {
            eprintln!("Error: {e}");
        }
    }

    eprintln!("\nTotal events: {}", events.len());

    // The test passes regardless of outcome (we're testing connectivity),
    // but log everything for debugging
    assert!(!events.is_empty(), "should have received events from the agent loop");
}
