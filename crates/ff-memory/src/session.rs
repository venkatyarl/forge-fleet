use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::store::{Memory, MemorySource, MemoryStore, NewMemory};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMemoryItem {
    pub content: String,
    pub tags: Vec<String>,
    pub source: MemorySource,
    pub importance: f32,
    pub created_at: DateTime<Utc>,
}

impl SessionMemoryItem {
    pub fn new(content: impl Into<String>, source: MemorySource) -> Self {
        Self {
            content: content.into(),
            tags: vec![],
            source,
            importance: 0.5,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClosureResult {
    pub session_id: String,
    pub workspace_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub summary: Option<Memory>,
    pub promoted_memories: Vec<Memory>,
    pub total_items: usize,
}

#[derive(Debug, Clone)]
struct SessionState {
    workspace_id: String,
    started_at: DateTime<Utc>,
    last_updated: DateTime<Utc>,
    items: Vec<SessionMemoryItem>,
}

#[derive(Debug)]
pub struct SessionMemoryManager {
    store: MemoryStore,
    sessions: DashMap<String, SessionState>,
    pub promote_threshold: f32,
    pub max_working_items: usize,
}

impl SessionMemoryManager {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            sessions: DashMap::new(),
            promote_threshold: 0.72,
            max_working_items: 300,
        }
    }

    pub fn start_session(&self, session_id: impl Into<String>, workspace_id: impl Into<String>) {
        let session_id = session_id.into();
        let workspace_id = workspace_id.into();
        let now = Utc::now();

        self.sessions.insert(
            session_id,
            SessionState {
                workspace_id,
                started_at: now,
                last_updated: now,
                items: vec![],
            },
        );
    }

    pub fn add_item(&self, session_id: &str, workspace_id: &str, mut item: SessionMemoryItem) {
        item.importance = item.importance.clamp(0.0, 1.0);

        let now = Utc::now();
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            session.last_updated = now;
            session.items.push(item);

            if session.items.len() > self.max_working_items {
                let drop_count = session.items.len() - self.max_working_items;
                session.items.drain(0..drop_count);
            }
            return;
        }

        self.sessions.insert(
            session_id.to_string(),
            SessionState {
                workspace_id: workspace_id.to_string(),
                started_at: now,
                last_updated: now,
                items: vec![item],
            },
        );
    }

    pub fn working_memory(&self, session_id: &str) -> Vec<SessionMemoryItem> {
        self.sessions
            .get(session_id)
            .map(|s| s.items.clone())
            .unwrap_or_default()
    }

    pub fn summarize_working_memory(&self, session_id: &str) -> Option<String> {
        let session = self.sessions.get(session_id)?;
        Some(summarize_items(&session.items, 12))
    }

    pub async fn end_session(&self, session_id: &str) -> Result<Option<SessionClosureResult>> {
        let (session_key, session) = match self.sessions.remove(session_id) {
            Some(pair) => pair,
            None => return Ok(None),
        };

        let ended_at = Utc::now();
        let summary_text = summarize_items(&session.items, 14);

        let summary = if summary_text.is_empty() {
            None
        } else {
            Some(
                self.store
                    .save_memory(NewMemory {
                        id: None,
                        workspace_id: session.workspace_id.clone(),
                        content: format!("Session {session_key} summary:\n{summary_text}"),
                        tags: vec![
                            "session_summary".to_string(),
                            format!("session:{session_key}"),
                        ],
                        source: MemorySource::Session,
                        importance: Some(0.78),
                        created_at: Some(ended_at),
                    })
                    .await?,
            )
        };

        let mut promoted = Vec::new();
        for item in session
            .items
            .iter()
            .filter(|i| i.importance >= self.promote_threshold)
        {
            let mut tags = item.tags.clone();
            tags.push("promoted_from_session".to_string());
            tags.push(format!("session:{session_key}"));

            let memory = self
                .store
                .save_memory(NewMemory {
                    id: None,
                    workspace_id: session.workspace_id.clone(),
                    content: item.content.clone(),
                    tags,
                    source: item.source,
                    importance: Some(item.importance),
                    created_at: Some(item.created_at),
                })
                .await?;
            promoted.push(memory);
        }

        Ok(Some(SessionClosureResult {
            session_id: session_key,
            workspace_id: session.workspace_id,
            started_at: session.started_at,
            ended_at,
            summary,
            promoted_memories: promoted,
            total_items: session.items.len(),
        }))
    }
}

fn summarize_items(items: &[SessionMemoryItem], max_items: usize) -> String {
    if items.is_empty() {
        return String::new();
    }

    let mut ranked = items.to_vec();
    ranked.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    ranked.truncate(max_items.max(1));

    let mut lines = Vec::with_capacity(ranked.len());
    for item in ranked {
        lines.push(format!(
            "- [{} | {:.2}] {}",
            item.source, item.importance, item.content
        ));
    }

    lines.join("\n")
}
