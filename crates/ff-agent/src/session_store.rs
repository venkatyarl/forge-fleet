//! Session persistence — save and resume agent sessions to/from disk.
//!
//! Sessions are stored as JSONL files in ~/.forgefleet/sessions/.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ff_api::tool_calling::ToolChatMessage;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

/// Directory where sessions are stored.
fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("sessions")
}

/// Metadata for a saved session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub model: String,
    pub llm_base_url: String,
    pub working_dir: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    pub turn_count: u32,
    /// First user message (truncated) for display.
    pub summary: String,
}

/// A persisted session (metadata + messages).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub meta: SessionMeta,
    pub messages: Vec<ToolChatMessage>,
}

/// Save a session to disk.
pub async fn save_session(
    session_id: &str,
    model: &str,
    llm_base_url: &str,
    working_dir: &str,
    messages: &[ToolChatMessage],
    turn_count: u32,
) -> anyhow::Result<()> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir).await?;

    let summary = messages
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.text_content())
        .map(|t| {
            if t.len() > 100 {
                format!("{}...", &t[..100])
            } else {
                t.to_string()
            }
        })
        .unwrap_or_else(|| "(no prompt)".into());

    let now = Utc::now();
    let persisted = PersistedSession {
        meta: SessionMeta {
            session_id: session_id.to_string(),
            model: model.to_string(),
            llm_base_url: llm_base_url.to_string(),
            working_dir: working_dir.to_string(),
            created_at: now,
            updated_at: now,
            message_count: messages.len(),
            turn_count,
            summary,
        },
        messages: messages.to_vec(),
    };

    let path = dir.join(format!("{session_id}.json"));
    let json = serde_json::to_string_pretty(&persisted)?;
    fs::write(&path, json).await?;

    Ok(())
}

/// Load a session from disk by ID.
pub async fn load_session(session_id: &str) -> anyhow::Result<PersistedSession> {
    let path = sessions_dir().join(format!("{session_id}.json"));
    let content = fs::read_to_string(&path).await?;
    let session: PersistedSession = serde_json::from_str(&content)?;
    Ok(session)
}

/// List all saved sessions (most recent first).
pub async fn list_sessions() -> Vec<SessionMeta> {
    let dir = sessions_dir();
    let mut sessions = Vec::new();

    let mut entries = match fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(_) => return sessions,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        match fs::read_to_string(&path).await {
            Ok(content) => {
                if let Ok(persisted) = serde_json::from_str::<PersistedSession>(&content) {
                    sessions.push(persisted.meta);
                }
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read session file");
            }
        }
    }

    // Sort by updated_at descending
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

/// Delete a saved session.
pub async fn delete_session(session_id: &str) -> anyhow::Result<()> {
    let path = sessions_dir().join(format!("{session_id}.json"));
    fs::remove_file(&path).await?;
    Ok(())
}
