//! LLM router trait — abstraction over selecting the next model and reporting failures.
//!
//! Implementations include the local inference router ([`ff_agent::coordinator::LocalLlmRouter`])
//! and the Pulse-backed fleet router ([`ff_gateway::llm_routing::PulseLlmRouter`]).
//! Keeping this as a trait lets the orchestrator swap routing strategies and
//! unit-test retry/escalation logic without spinning up real infrastructure.

use async_trait::async_trait;
use ff_core::Tier;
use serde::{Deserialize, Serialize};

// ─── Candidate ───────────────────────────────────────────────────────────────

/// A concrete LLM selected by the router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmCandidate {
    /// Model identifier (e.g. `qwen3-32b`).
    pub model_id: String,
    /// Human-readable model name.
    pub model_name: String,
    /// Inference endpoint URL for this model.
    pub endpoint: String,
    /// Model tier.
    pub tier: Tier,
}

// ─── Failure reporting ───────────────────────────────────────────────────────

/// Why an LLM call failed.
///
/// Routers use this to update health scores, circuit breakers, and exclusion lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LlmFailureKind {
    /// The model refused the request or produced an unparseable/invalid response.
    Refusal,
    /// The request timed out before producing a response.
    Timeout,
    /// The upstream returned an error status or was unreachable.
    Unavailable,
    /// The response quality was too low (e.g. failed validation or judge gate).
    Quality,
}

// ─── Router trait ────────────────────────────────────────────────────────────

/// Abstraction over selecting the next LLM and reporting failures.
#[async_trait]
pub trait LlmRouter: Send + Sync {
    /// Select the best available LLM for the current request.
    ///
    /// `preferred_tier` is the ideal tier for the task; the router may return a
    /// higher or lower tier depending on availability, constraints, and recent
    /// failures. Returns `None` when no model is routable.
    async fn select_next_llm(&self, preferred_tier: Tier) -> Option<LlmCandidate>;

    /// Report that a previously selected candidate failed.
    ///
    /// Implementations should use this to update internal health state so the
    /// next `select_next_llm` call can avoid recently-failing models/nodes.
    async fn report_failure(&self, candidate: &LlmCandidate, kind: LlmFailureKind);
}
