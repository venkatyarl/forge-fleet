//! Session message history.
//!
//! Stores conversation history per session with:
//! - append and retrieval
//! - pagination
//! - text search
//! - export (JSON/Markdown)
//! - compaction for long sessions

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;
use uuid::Uuid;

/// Message role within a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
    SubAgent,
    Developer,
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => write!(f, "system"),
            Self::User => write!(f, "user"),
            Self::Assistant => write!(f, "assistant"),
            Self::Tool => write!(f, "tool"),
            Self::SubAgent => write!(f, "subagent"),
            Self::Developer => write!(f, "developer"),
        }
    }
}

/// A single history entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEntry {
    pub id: Uuid,
    pub role: MessageRole,
    pub content: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub tokens: usize,
    pub created_at: DateTime<Utc>,
}

impl MessageEntry {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        let content = content.into();
        let tokens = estimate_tokens(&content);
        Self {
            id: Uuid::new_v4(),
            role,
            content,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            tokens,
            created_at: Utc::now(),
        }
    }
}

/// Search result hit for message history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub message_id: Uuid,
    pub role: MessageRole,
    pub score: f32,
    pub snippet: String,
    pub created_at: DateTime<Utc>,
}

/// Per-session history stats.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoryStats {
    pub message_count: usize,
    pub total_tokens: usize,
    pub oldest: Option<DateTime<Utc>>,
    pub newest: Option<DateTime<Utc>>,
}

/// Concurrent message history store.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    sessions: Arc<DashMap<Uuid, Vec<MessageEntry>>>,
    max_messages_before_compaction: usize,
}

impl HistoryStore {
    /// Create a store with max messages threshold for auto-compaction.
    pub fn new(max_messages_before_compaction: usize) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            max_messages_before_compaction,
        }
    }

    /// Append a message and return the stored entry.
    pub fn append(
        &self,
        session_id: Uuid,
        role: MessageRole,
        content: impl Into<String>,
        metadata: Option<serde_json::Value>,
    ) -> MessageEntry {
        let mut entry = MessageEntry::new(role, content);
        if let Some(metadata) = metadata {
            entry.metadata = metadata;
        }
        self.append_entry(session_id, entry.clone());
        entry
    }

    /// Append a pre-built message entry.
    pub fn append_entry(&self, session_id: Uuid, entry: MessageEntry) {
        self.sessions.entry(session_id).or_default().push(entry);

        if self.len(session_id) > self.max_messages_before_compaction {
            let _ = self.compact(session_id, self.max_messages_before_compaction / 2);
        }
    }

    /// Get messages with pagination.
    pub fn get_paginated(
        &self,
        session_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Vec<MessageEntry> {
        let Some(entries) = self.sessions.get(&session_id) else {
            return Vec::new();
        };

        let len = entries.len();
        if offset >= len {
            return Vec::new();
        }

        let end = (offset + limit).min(len);
        entries[offset..end].to_vec()
    }

    /// Get the most recent `limit` messages.
    pub fn recent(&self, session_id: Uuid, limit: usize) -> Vec<MessageEntry> {
        let Some(entries) = self.sessions.get(&session_id) else {
            return Vec::new();
        };

        let len = entries.len();
        if limit >= len {
            return entries.clone();
        }

        entries[len - limit..].to_vec()
    }

    /// Search messages by text query with basic term-frequency scoring.
    pub fn search(&self, session_id: Uuid, query: &str, limit: usize) -> Vec<SearchHit> {
        let Some(entries) = self.sessions.get(&session_id) else {
            return Vec::new();
        };
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        if terms.is_empty() {
            return Vec::new();
        }

        let mut hits = Vec::new();
        for msg in entries.iter() {
            let content_lc = msg.content.to_lowercase();
            let mut score = 0.0f32;
            for term in &terms {
                let count = content_lc.matches(term).count() as f32;
                score += count;
            }

            if score > 0.0 {
                let snippet = make_snippet(&msg.content, &terms[0], 160);
                hits.push(SearchHit {
                    message_id: msg.id,
                    role: msg.role,
                    score,
                    snippet,
                    created_at: msg.created_at,
                });
            }
        }

        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        hits
    }

    /// Export full history for a session as pretty JSON.
    pub fn export_json(&self, session_id: Uuid) -> anyhow::Result<String> {
        let entries = self
            .sessions
            .get(&session_id)
            .map(|r| r.value().clone())
            .unwrap_or_default();
        Ok(serde_json::to_string_pretty(&entries)?)
    }

    /// Export full history for a session as Markdown transcript.
    pub fn export_markdown(&self, session_id: Uuid) -> String {
        let entries = self
            .sessions
            .get(&session_id)
            .map(|r| r.value().clone())
            .unwrap_or_default();

        let mut out = String::new();
        for msg in entries {
            out.push_str(&format!(
                "## [{}] {}\n\n{}\n\n",
                msg.role,
                msg.created_at.to_rfc3339(),
                msg.content
            ));
        }
        out
    }

    /// Compact a long session by summarizing older messages into one system entry.
    ///
    /// Keeps the newest `keep_last` messages and replaces older messages with a
    /// generated summary message.
    pub fn compact(&self, session_id: Uuid, keep_last: usize) -> Option<MessageEntry> {
        let mut entries = self.sessions.get_mut(&session_id)?;
        if entries.len() <= keep_last || keep_last == 0 {
            return None;
        }

        let split = entries.len().saturating_sub(keep_last);
        if split == 0 {
            return None;
        }

        let old: Vec<MessageEntry> = entries.drain(0..split).collect();
        if old.is_empty() {
            return None;
        }

        let old_count = old.len();
        let old_tokens: usize = old.iter().map(|m| m.tokens).sum();

        let mut summary_lines = Vec::new();
        for msg in old.iter().rev().take(24).rev() {
            let mut line = msg.content.replace('\n', " ");
            if line.len() > 180 {
                line.truncate(180);
                line.push('…');
            }
            summary_lines.push(format!("- {}: {}", msg.role, line));
        }

        let content = format!(
            "Compacted {} earlier messages ({} estimated tokens).\n\nRecent highlights:\n{}",
            old_count,
            old_tokens,
            summary_lines.join("\n")
        );

        let mut summary = MessageEntry::new(MessageRole::System, content);
        summary.metadata = json!({
            "compacted": true,
            "compacted_count": old_count,
            "compacted_tokens": old_tokens,
        });

        entries.insert(0, summary.clone());
        info!(session_id = %session_id, old_count, "history compacted");
        Some(summary)
    }

    /// Compact until total tokens are below `max_tokens` while preserving at
    /// least `keep_last_min` messages.
    pub fn compact_to_token_budget(
        &self,
        session_id: Uuid,
        max_tokens: usize,
        keep_last_min: usize,
    ) -> bool {
        if self.total_tokens(session_id) <= max_tokens {
            return false;
        }

        // Keep at least N last messages and compact everything before it.
        let keep_last = keep_last_min.max(1);
        self.compact(session_id, keep_last).is_some()
    }

    /// Delete all history for a session.
    pub fn clear(&self, session_id: Uuid) -> bool {
        self.sessions.remove(&session_id).is_some()
    }

    /// Number of messages in a session.
    pub fn len(&self, session_id: Uuid) -> usize {
        self.sessions.get(&session_id).map(|v| v.len()).unwrap_or(0)
    }

    /// Total estimated tokens in a session.
    pub fn total_tokens(&self, session_id: Uuid) -> usize {
        self.sessions
            .get(&session_id)
            .map(|v| v.iter().map(|m| m.tokens).sum())
            .unwrap_or(0)
    }

    /// Stats for a session history.
    pub fn stats(&self, session_id: Uuid) -> HistoryStats {
        let Some(entries) = self.sessions.get(&session_id) else {
            return HistoryStats::default();
        };

        let message_count = entries.len();
        let total_tokens: usize = entries.iter().map(|m| m.tokens).sum();
        let oldest = entries.first().map(|m| m.created_at);
        let newest = entries.last().map(|m| m.created_at);

        HistoryStats {
            message_count,
            total_tokens,
            oldest,
            newest,
        }
    }

    /// Number of sessions tracked.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::new(300)
    }
}

/// Very rough token estimate suitable for budgeting heuristics.
fn estimate_tokens(text: &str) -> usize {
    // Rule of thumb for English text: ~4 chars/token.
    (text.chars().count() / 4).max(1)
}

fn make_snippet(text: &str, needle_lc: &str, max_chars: usize) -> String {
    let lower = text.to_lowercase();
    if let Some(idx) = lower.find(needle_lc) {
        let start = idx.saturating_sub(max_chars / 4);
        let end = (start + max_chars).min(text.len());
        text[start..end].to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_paginate() {
        let store = HistoryStore::new(100);
        let sid = Uuid::new_v4();
        store.append(sid, MessageRole::User, "hello", None);
        store.append(sid, MessageRole::Assistant, "hi there", None);
        store.append(sid, MessageRole::User, "how are you", None);

        let page = store.get_paginated(sid, 1, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].role, MessageRole::Assistant);
    }

    #[test]
    fn search_hits() {
        let store = HistoryStore::new(100);
        let sid = Uuid::new_v4();
        store.append(sid, MessageRole::User, "deploy pipeline failed", None);
        store.append(
            sid,
            MessageRole::Assistant,
            "check CI logs for failure",
            None,
        );

        let hits = store.search(sid, "failed", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn compact_history() {
        let store = HistoryStore::new(1000);
        let sid = Uuid::new_v4();
        for i in 0..20 {
            store.append(sid, MessageRole::User, format!("message {i}"), None);
        }

        let summary = store.compact(sid, 5).expect("should compact");
        assert_eq!(summary.role, MessageRole::System);
        assert_eq!(store.len(sid), 6); // summary + 5 retained
    }

    #[test]
    fn export_json_works() {
        let store = HistoryStore::new(100);
        let sid = Uuid::new_v4();
        store.append(sid, MessageRole::User, "hello", None);

        let json = store.export_json(sid).unwrap();
        assert!(json.contains("hello"));
    }
}
