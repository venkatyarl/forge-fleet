//! Context window management.
//!
//! Tracks per-session context usage and budget.
//! Integrates with [`HistoryStore`] to trigger compaction and build model-ready
//! context payloads with injected system prompts.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use uuid::Uuid;

use crate::history::{HistoryStore, MessageEntry, MessageRole};

/// Context budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBudget {
    /// Total context window size for input+output.
    pub max_tokens: usize,
    /// Reserved tokens for model output.
    pub reserve_for_output: usize,
    /// Usage threshold (0.0..=1.0) at which compaction should trigger.
    pub compaction_threshold: f32,
    /// Minimum number of most recent messages to keep during compaction.
    pub min_messages_after_compaction: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            reserve_for_output: 4_096,
            compaction_threshold: 0.80,
            min_messages_after_compaction: 24,
        }
    }
}

impl ContextBudget {
    /// Max input tokens available (total minus reserved output).
    pub fn max_input_tokens(&self) -> usize {
        self.max_tokens.saturating_sub(self.reserve_for_output)
    }
}

/// Runtime context stats for observability.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextStats {
    pub used_tokens: usize,
    pub available_tokens: usize,
    pub usage_ratio: f32,
    pub message_count: usize,
    pub compaction_count: u64,
    pub last_compacted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct SessionContextState {
    budget: ContextBudget,
    used_tokens: usize,
    compaction_count: u64,
    last_compacted_at: Option<DateTime<Utc>>,
    system_prompts: Vec<String>,
}

impl SessionContextState {
    fn new(budget: ContextBudget) -> Self {
        Self {
            budget,
            used_tokens: 0,
            compaction_count: 0,
            last_compacted_at: None,
            system_prompts: Vec::new(),
        }
    }

    fn usage_ratio(&self) -> f32 {
        let cap = self.budget.max_input_tokens() as f32;
        if cap <= 0.0 {
            1.0
        } else {
            (self.used_tokens as f32 / cap).clamp(0.0, 1.0)
        }
    }

    fn should_compact(&self) -> bool {
        self.usage_ratio() >= self.budget.compaction_threshold
    }
}

/// Tracks context budget per session.
#[derive(Debug, Clone)]
pub struct ContextManager {
    sessions: Arc<DashMap<Uuid, SessionContextState>>,
    default_budget: ContextBudget,
}

impl ContextManager {
    pub fn new(default_budget: ContextBudget) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            default_budget,
        }
    }

    /// Register or reset a session context with optional custom budget.
    pub fn register_session(&self, session_id: Uuid, budget: Option<ContextBudget>) {
        let budget = budget.unwrap_or_else(|| self.default_budget.clone());
        self.sessions
            .insert(session_id, SessionContextState::new(budget));
    }

    /// Remove session context tracking.
    pub fn remove_session(&self, session_id: Uuid) -> bool {
        self.sessions.remove(&session_id).is_some()
    }

    /// Set/override context budget for a session.
    pub fn set_budget(&self, session_id: Uuid, budget: ContextBudget) {
        self.sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()))
            .budget = budget;
    }

    /// Get current budget for a session.
    pub fn budget(&self, session_id: Uuid) -> ContextBudget {
        self.sessions
            .get(&session_id)
            .map(|s| s.budget.clone())
            .unwrap_or_else(|| self.default_budget.clone())
    }

    /// Add a system prompt to be injected in built context for this session.
    pub fn inject_system_prompt(&self, session_id: Uuid, prompt: impl Into<String>) {
        let prompt = prompt.into();
        self.sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()))
            .system_prompts
            .push(prompt);
    }

    /// Replace all system prompts for a session.
    pub fn set_system_prompts(&self, session_id: Uuid, prompts: Vec<String>) {
        self.sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()))
            .system_prompts = prompts;
    }

    /// Clear injected system prompts.
    pub fn clear_system_prompts(&self, session_id: Uuid) {
        if let Some(mut s) = self.sessions.get_mut(&session_id) {
            s.system_prompts.clear();
        }
    }

    /// Get system prompts currently configured for a session.
    pub fn system_prompts(&self, session_id: Uuid) -> Vec<String> {
        self.sessions
            .get(&session_id)
            .map(|s| s.system_prompts.clone())
            .unwrap_or_default()
    }

    /// Sync tracked token usage from history store.
    pub fn sync_from_history(&self, session_id: Uuid, history: &HistoryStore) {
        let used = history.total_tokens(session_id);
        let mut entry = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()));
        entry.used_tokens = used;
    }

    /// Record incremental token usage.
    pub fn record_tokens(&self, session_id: Uuid, tokens: usize) {
        let mut state = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()));
        state.used_tokens = state.used_tokens.saturating_add(tokens);
    }

    /// Whether a session should be compacted based on budget threshold.
    pub fn should_compact(&self, session_id: Uuid) -> bool {
        self.sessions
            .get(&session_id)
            .map(|s| s.should_compact())
            .unwrap_or(false)
    }

    /// Trigger compaction if needed.
    ///
    /// Returns true if compaction happened.
    pub fn compact_if_needed(&self, session_id: Uuid, history: &HistoryStore) -> bool {
        self.sync_from_history(session_id, history);

        let should = self.should_compact(session_id);
        if !should {
            return false;
        }

        let mut state = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()));

        let target = ((state.budget.max_input_tokens() as f32) * 0.55).round() as usize;
        let compacted = history.compact_to_token_budget(
            session_id,
            target,
            state.budget.min_messages_after_compaction,
        );

        if compacted {
            state.used_tokens = history.total_tokens(session_id);
            state.compaction_count += 1;
            state.last_compacted_at = Some(Utc::now());
            info!(
                session_id = %session_id,
                used_tokens = state.used_tokens,
                compaction_count = state.compaction_count,
                "context compacted"
            );
        }

        compacted
    }

    /// Build a model-ready context window:
    /// - injected system prompts first
    /// - then as many recent history messages as fit in budget
    pub fn build_context(
        &self,
        session_id: Uuid,
        history: &HistoryStore,
        max_messages: usize,
    ) -> Vec<MessageEntry> {
        self.sync_from_history(session_id, history);

        let state = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionContextState::new(self.default_budget.clone()));

        let budget = state.budget.max_input_tokens();
        let mut out = Vec::new();
        let mut used = 0usize;

        for prompt in &state.system_prompts {
            let entry = MessageEntry::new(MessageRole::System, prompt.clone());
            used = used.saturating_add(entry.tokens);
            out.push(entry);
        }

        let recent = history.recent(session_id, max_messages);
        let mut selected_rev = Vec::new();
        for msg in recent.into_iter().rev() {
            if used + msg.tokens > budget {
                break;
            }
            used += msg.tokens;
            selected_rev.push(msg);
        }

        selected_rev.reverse();
        out.extend(selected_rev);

        debug!(session_id = %session_id, messages = out.len(), used_tokens = used, "context built");
        out
    }

    /// Context stats for a session.
    pub fn stats(&self, session_id: Uuid, history: &HistoryStore) -> ContextStats {
        let used_tokens = history.total_tokens(session_id);
        let message_count = history.len(session_id);
        let state = self
            .sessions
            .get(&session_id)
            .map(|s| s.clone())
            .unwrap_or_else(|| SessionContextState::new(self.default_budget.clone()));

        let cap = state.budget.max_input_tokens();
        let available_tokens = cap.saturating_sub(used_tokens);
        let usage_ratio = if cap == 0 {
            1.0
        } else {
            (used_tokens as f32 / cap as f32).clamp(0.0, 1.0)
        };

        ContextStats {
            used_tokens,
            available_tokens,
            usage_ratio,
            message_count,
            compaction_count: state.compaction_count,
            last_compacted_at: state.last_compacted_at,
        }
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::new(ContextBudget::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::MessageRole;

    #[test]
    fn usage_and_compaction() {
        let history = HistoryStore::new(10_000);
        let manager = ContextManager::new(ContextBudget {
            max_tokens: 200,
            reserve_for_output: 50,
            compaction_threshold: 0.5,
            min_messages_after_compaction: 4,
        });

        let sid = Uuid::new_v4();
        manager.register_session(sid, None);

        for i in 0..50 {
            history.append(
                sid,
                MessageRole::User,
                format!("message {i} with enough words to consume some tokens"),
                None,
            );
        }

        manager.sync_from_history(sid, &history);
        assert!(manager.should_compact(sid));
        let compacted = manager.compact_if_needed(sid, &history);
        assert!(compacted);

        let stats = manager.stats(sid, &history);
        assert!(stats.compaction_count >= 1);
    }

    #[test]
    fn builds_context_with_system_prompt() {
        let history = HistoryStore::new(1000);
        let manager = ContextManager::default();
        let sid = Uuid::new_v4();

        manager.register_session(sid, None);
        manager.inject_system_prompt(sid, "You are ForgeFleet assistant.");

        history.append(sid, MessageRole::User, "hello", None);
        history.append(sid, MessageRole::Assistant, "hi", None);

        let ctx = manager.build_context(sid, &history, 20);
        assert!(!ctx.is_empty());
        assert_eq!(ctx[0].role, MessageRole::System);
    }
}
