//! Static configuration for the offline-mode small language model (SLM).
//!
//! When the fleet loses connectivity the agent falls back to a lightweight,
//! locally-runnable model instead of a cloud CLI or a remote fleet node. This
//! module centralises the constants that describe *which* model to load and
//! *when* to fall back to it, so the offline runner and the `slm` client agree
//! on a single set of defaults.
//!
//! These are compile-time defaults only; runtime callers may still override the
//! actual model path via `FORGEFLEET_SLM_MODEL` (see [`crate::slm`]).

/// Default lightweight model to load when connectivity is lost.
///
/// Phi-3-mini-4k-instruct is small enough to run on a laptop-class node while
/// still handling the bounded classification/rewrite work the offline runner
/// dispatches.
pub const DEFAULT_OFFLINE_MODEL_NAME: &str = "Phi-3-mini-4k-instruct";

/// Default quantization for the offline model (4-bit, medium k-quant).
///
/// Matches the largest quantization the [`crate::slm`] client accepts, keeping
/// the on-disk footprint and RAM budget small enough for offline use.
pub const DEFAULT_OFFLINE_QUANTIZATION: &str = "Q4_K_M";

/// Canonical GGUF file name for the default offline model, combining the model
/// name and quantization (e.g. `Phi-3-mini-4k-instruct-Q4_K_M.gguf`).
pub const DEFAULT_OFFLINE_MODEL_FILE: &str = "Phi-3-mini-4k-instruct-Q4_K_M.gguf";

/// Default context window (tokens) to request when loading the offline model.
pub const DEFAULT_OFFLINE_CONTEXT_TOKENS: u32 = 4096;

/// Default RAM budget (MiB) reserved for the offline model, kept conservative
/// so a fallback never starves the rest of the node.
pub const DEFAULT_OFFLINE_MEM_BUDGET_MB: u64 = 4096;

/// Conditions that cause the agent to fall back to the local offline SLM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackTrigger {
    /// The node has no network connectivity at all.
    ConnectivityLost,
    /// The fleet leader / control plane cannot be reached.
    LeaderUnreachable,
    /// No cloud CLI (claude/codex/kimi/...) or remote fleet model responded.
    CloudUnavailable,
    /// An operator explicitly forced offline mode.
    ManualOverride,
}

impl FallbackTrigger {
    /// Stable, lowercase identifier for logs and metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            FallbackTrigger::ConnectivityLost => "connectivity_lost",
            FallbackTrigger::LeaderUnreachable => "leader_unreachable",
            FallbackTrigger::CloudUnavailable => "cloud_unavailable",
            FallbackTrigger::ManualOverride => "manual_override",
        }
    }
}

/// The set of triggers that activate the offline SLM by default. Manual
/// override is intentionally excluded so that automatic fallback and operator
/// intent can be distinguished by callers.
pub const DEFAULT_FALLBACK_TRIGGERS: &[FallbackTrigger] = &[
    FallbackTrigger::ConnectivityLost,
    FallbackTrigger::LeaderUnreachable,
    FallbackTrigger::CloudUnavailable,
];

/// Resolved offline-mode SLM configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineSlmConfig {
    /// Human-readable model name.
    pub model_name: String,
    /// Quantization level (e.g. `Q4_K_M`).
    pub quantization: String,
    /// GGUF file name expected on disk.
    pub model_file: String,
    /// Context window in tokens.
    pub context_tokens: u32,
    /// RAM budget in MiB.
    pub mem_budget_mb: u64,
    /// Triggers that activate the offline model.
    pub triggers: Vec<FallbackTrigger>,
}

impl Default for OfflineSlmConfig {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_OFFLINE_MODEL_NAME.to_string(),
            quantization: DEFAULT_OFFLINE_QUANTIZATION.to_string(),
            model_file: DEFAULT_OFFLINE_MODEL_FILE.to_string(),
            context_tokens: DEFAULT_OFFLINE_CONTEXT_TOKENS,
            mem_budget_mb: DEFAULT_OFFLINE_MEM_BUDGET_MB,
            triggers: DEFAULT_FALLBACK_TRIGGERS.to_vec(),
        }
    }
}

impl OfflineSlmConfig {
    /// Returns `true` if `trigger` should activate the offline model under this
    /// configuration.
    pub fn triggers_fallback(&self, trigger: FallbackTrigger) -> bool {
        self.triggers.contains(&trigger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_targets_a_4bit_phi3_mini() {
        let config = OfflineSlmConfig::default();
        assert_eq!(config.model_name, "Phi-3-mini-4k-instruct");
        assert_eq!(config.quantization, "Q4_K_M");
        assert!(config.model_file.starts_with(&config.model_name));
        assert!(config.model_file.ends_with(".gguf"));
    }

    #[test]
    fn automatic_triggers_fall_back_but_manual_override_does_not() {
        let config = OfflineSlmConfig::default();
        assert!(config.triggers_fallback(FallbackTrigger::ConnectivityLost));
        assert!(config.triggers_fallback(FallbackTrigger::LeaderUnreachable));
        assert!(config.triggers_fallback(FallbackTrigger::CloudUnavailable));
        assert!(!config.triggers_fallback(FallbackTrigger::ManualOverride));
    }

    #[test]
    fn trigger_identifiers_are_stable() {
        assert_eq!(
            FallbackTrigger::ConnectivityLost.as_str(),
            "connectivity_lost"
        );
        assert_eq!(
            FallbackTrigger::LeaderUnreachable.as_str(),
            "leader_unreachable"
        );
        assert_eq!(
            FallbackTrigger::CloudUnavailable.as_str(),
            "cloud_unavailable"
        );
        assert_eq!(FallbackTrigger::ManualOverride.as_str(), "manual_override");
    }
}
