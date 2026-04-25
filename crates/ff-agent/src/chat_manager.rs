//! Chat manager — multi-chat sessions with project-scoped memory.
//!
//! Manages the lifecycle of chat sessions:
//! - Each project can have multiple chats
//! - Each chat has its own conversation history, Focus Stack, and Backlog
//! - All chats within a project share that project's memory scope
//! - Chats are persisted and can be resumed
//!
//! Navigation:
//! - Main menu → "Chats" → lists all chats (filterable by scope)
//! - Project → "Chat" → lists chats for that project, can create new
//! - Opening a chat from a project defaults to that project's scope

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{info, warn};

use crate::focus_stack::ConversationTracker;
use crate::scoped_memory::MemoryScope;

// ---------------------------------------------------------------------------
// Chat session model
// ---------------------------------------------------------------------------

/// A chat session — one conversation with an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    /// Unique chat ID.
    pub id: String,
    /// Display name (user can rename).
    pub name: String,
    /// Memory scope this chat operates in.
    pub scope: MemoryScope,
    /// LLM endpoint used.
    pub llm_base_url: String,
    /// Model name.
    pub model: String,
    /// Working directory for tool execution.
    pub working_dir: PathBuf,
    /// Chat status.
    pub status: ChatStatus,
    /// When the chat was created.
    pub created_at: DateTime<Utc>,
    /// When the chat was last active.
    pub last_active_at: DateTime<Utc>,
    /// Number of messages in this chat.
    pub message_count: usize,
    /// Number of turns completed.
    pub turn_count: u32,
    /// First user message (for preview).
    pub preview: String,
    /// Focus Stack + Backlog state.
    pub tracker: ConversationTracker,
    /// Model selection mode.
    pub model_selection: ModelSelection,
    /// Tags for organization.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatStatus {
    /// Currently active.
    Active,
    /// Paused (user navigated away).
    Paused,
    /// Completed (user explicitly ended).
    Completed,
    /// Archived (old, kept for reference).
    Archived,
}

/// How the model is selected for this chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelSelection {
    /// ForgeFleet auto-selects based on the question/task.
    Auto,
    /// User explicitly chose a specific model/endpoint.
    Manual { model: String, llm_base_url: String },
    /// Route to the best model for the task type.
    TaskBased {
        task_type: String, // "coding", "reasoning", "review", "fast"
    },
}

impl Default for ModelSelection {
    fn default() -> Self {
        Self::Auto
    }
}

// ---------------------------------------------------------------------------
// Chat list for navigation
// ---------------------------------------------------------------------------

/// A chat list entry (for sidebar/menu display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatListEntry {
    pub id: String,
    pub name: String,
    pub scope_display: String,
    pub status: ChatStatus,
    pub last_active_at: DateTime<Utc>,
    pub message_count: usize,
    pub preview: String,
    pub stack_depth: usize,
    pub backlog_count: usize,
}

// ---------------------------------------------------------------------------
// Chat manager
// ---------------------------------------------------------------------------

/// Manages all chat sessions across all scopes.
pub struct ChatManager {
    /// All known chats keyed by ID.
    chats: HashMap<String, ChatSession>,
    /// Index: project_id → list of chat IDs.
    project_chats: HashMap<String, Vec<String>>,
    /// Index: scope hash → list of chat IDs.
    scope_chats: HashMap<String, Vec<String>>,
}

impl ChatManager {
    pub fn new() -> Self {
        Self {
            chats: HashMap::new(),
            project_chats: HashMap::new(),
            scope_chats: HashMap::new(),
        }
    }

    /// Load all chat sessions from disk.
    pub async fn load() -> Self {
        let mut manager = Self::new();
        let chats_dir = chats_base_dir();

        if let Ok(mut entries) = fs::read_dir(&chats_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(&path).await {
                    if let Ok(chat) = serde_json::from_str::<ChatSession>(&content) {
                        manager.index_chat(&chat);
                        manager.chats.insert(chat.id.clone(), chat);
                    }
                }
            }
        }

        info!(count = manager.chats.len(), "loaded chat sessions");
        manager
    }

    fn index_chat(&mut self, chat: &ChatSession) {
        // Index by project
        if let MemoryScope::Project { project_id, .. } = &chat.scope {
            self.project_chats
                .entry(project_id.clone())
                .or_default()
                .push(chat.id.clone());
        }

        // Index by scope
        let scope_key = scope_hash(&chat.scope);
        self.scope_chats
            .entry(scope_key)
            .or_default()
            .push(chat.id.clone());
    }

    /// Create a new chat session.
    pub async fn create(
        &mut self,
        name: Option<String>,
        scope: MemoryScope,
        llm_base_url: String,
        model: String,
        working_dir: PathBuf,
        model_selection: ModelSelection,
    ) -> ChatSession {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();

        let chat_name = name.unwrap_or_else(|| format!("Chat {}", now.format("%b %d %H:%M")));

        let chat = ChatSession {
            id: id.clone(),
            name: chat_name,
            scope: scope.clone(),
            llm_base_url,
            model,
            working_dir,
            status: ChatStatus::Active,
            created_at: now,
            last_active_at: now,
            message_count: 0,
            turn_count: 0,
            preview: String::new(),
            tracker: ConversationTracker::new(),
            model_selection,
            tags: Vec::new(),
        };

        self.index_chat(&chat);
        self.chats.insert(id.clone(), chat.clone());

        // Persist
        if let Err(e) = self.save_chat(&chat).await {
            warn!(id = %id, error = %e, "failed to save new chat");
        }

        info!(id = %id, scope = %scope.display_name(), "created chat session");
        chat
    }

    /// Create a chat from within a project (auto-scoped).
    pub async fn create_for_project(
        &mut self,
        project_id: &str,
        project_name: &str,
        llm_base_url: String,
        working_dir: PathBuf,
    ) -> ChatSession {
        let scope = MemoryScope::Project {
            project_id: project_id.to_string(),
            project_name: project_name.to_string(),
        };

        self.create(
            None,
            scope,
            llm_base_url,
            "auto".into(),
            working_dir,
            ModelSelection::Auto,
        )
        .await
    }

    /// Get a chat by ID.
    pub fn get(&self, id: &str) -> Option<&ChatSession> {
        self.chats.get(id)
    }

    /// Get mutable reference to a chat.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut ChatSession> {
        self.chats.get_mut(id)
    }

    /// List all chats for a project (most recent first).
    pub fn list_for_project(&self, project_id: &str) -> Vec<ChatListEntry> {
        let ids = self
            .project_chats
            .get(project_id)
            .cloned()
            .unwrap_or_default();
        let mut entries: Vec<ChatListEntry> = ids
            .iter()
            .filter_map(|id| self.chats.get(id))
            .map(chat_to_list_entry)
            .collect();
        entries.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
        entries
    }

    /// List all chats across all scopes (most recent first).
    pub fn list_all(&self) -> Vec<ChatListEntry> {
        let mut entries: Vec<ChatListEntry> = self.chats.values().map(chat_to_list_entry).collect();
        entries.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
        entries
    }

    /// List chats by scope type.
    pub fn list_by_scope(&self, scope_type: &str) -> Vec<ChatListEntry> {
        self.chats
            .values()
            .filter(|c| match (&c.scope, scope_type) {
                (MemoryScope::Global, "global") => true,
                (MemoryScope::Project { .. }, "project") => true,
                (MemoryScope::Folder { .. }, "folder") => true,
                (MemoryScope::Temp { .. }, "temp") => true,
                _ => false,
            })
            .map(chat_to_list_entry)
            .collect()
    }

    /// Update a chat's status.
    pub async fn update_status(&mut self, id: &str, status: ChatStatus) {
        if let Some(chat) = self.chats.get_mut(id) {
            chat.status = status;
            chat.last_active_at = Utc::now();
        }
        self.persist(id).await;
    }

    /// Update chat metadata after a turn.
    pub async fn record_turn(
        &mut self,
        id: &str,
        message_count: usize,
        turn_count: u32,
        preview: &str,
    ) {
        if let Some(chat) = self.chats.get_mut(id) {
            chat.message_count = message_count;
            chat.turn_count = turn_count;
            chat.last_active_at = Utc::now();
            if chat.preview.is_empty() && !preview.is_empty() {
                chat.preview = if preview.len() > 100 {
                    format!("{}...", &preview[..100])
                } else {
                    preview.to_string()
                };
            }
        }
        self.persist(id).await;
    }

    /// Rename a chat.
    pub async fn rename(&mut self, id: &str, new_name: &str) {
        if let Some(chat) = self.chats.get_mut(id) {
            chat.name = new_name.to_string();
        }
        self.persist(id).await;
    }

    /// Move a chat into a folder.
    pub async fn move_to_folder(&mut self, id: &str, folder_path: &str) {
        if let Some(chat) = self.chats.get_mut(id) {
            chat.tags.retain(|t| !t.starts_with("folder:"));
            chat.tags.push(format!("folder:{folder_path}"));
        }
        self.persist(id).await;
    }

    /// Persist a chat to disk (helper to avoid borrow conflicts).
    async fn persist(&self, id: &str) {
        if let Some(chat) = self.chats.get(id) {
            let _ = self.save_chat(chat).await;
        }
    }

    /// Archive a chat.
    pub async fn archive(&mut self, id: &str) {
        self.update_status(id, ChatStatus::Archived).await;
    }

    /// Delete a chat (removes from disk).
    pub async fn delete(&mut self, id: &str) {
        self.chats.remove(id);
        // Clean up indexes
        for list in self.project_chats.values_mut() {
            list.retain(|i| i != id);
        }
        for list in self.scope_chats.values_mut() {
            list.retain(|i| i != id);
        }

        let path = chats_base_dir().join(format!("{id}.json"));
        let _ = fs::remove_file(&path).await;
    }

    /// Save a chat to disk.
    async fn save_chat(&self, chat: &ChatSession) -> anyhow::Result<()> {
        let dir = chats_base_dir();
        fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", chat.id));
        let json = serde_json::to_string_pretty(chat)?;
        fs::write(&path, json).await?;
        Ok(())
    }

    /// List chats in a specific folder (supports nested folders like "work/frontend").
    pub fn list_in_folder(&self, folder_path: &str) -> Vec<ChatListEntry> {
        let tag = format!("folder:{folder_path}");
        self.chats
            .values()
            .filter(|c| c.tags.iter().any(|t| t == &tag))
            .map(chat_to_list_entry)
            .collect()
    }

    /// List all folder paths that have chats.
    pub fn list_folders(&self) -> Vec<String> {
        let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for chat in self.chats.values() {
            for tag in &chat.tags {
                if let Some(path) = tag.strip_prefix("folder:") {
                    folders.insert(path.to_string());
                    // Also add parent folders
                    let mut parts: Vec<&str> = path.split('/').collect();
                    while parts.len() > 1 {
                        parts.pop();
                        folders.insert(parts.join("/"));
                    }
                }
            }
        }
        folders.into_iter().collect()
    }

    /// List chats that are NOT in any folder.
    pub fn list_unfiled(&self) -> Vec<ChatListEntry> {
        self.chats
            .values()
            .filter(|c| !c.tags.iter().any(|t| t.starts_with("folder:")))
            .map(chat_to_list_entry)
            .collect()
    }

    /// Count chats by status.
    pub fn stats(&self) -> ChatStats {
        let mut stats = ChatStats::default();
        for chat in self.chats.values() {
            match chat.status {
                ChatStatus::Active => stats.active += 1,
                ChatStatus::Paused => stats.paused += 1,
                ChatStatus::Completed => stats.completed += 1,
                ChatStatus::Archived => stats.archived += 1,
            }
        }
        stats.total = self.chats.len();
        stats
    }
}

impl Default for ChatManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatStats {
    pub total: usize,
    pub active: usize,
    pub paused: usize,
    pub completed: usize,
    pub archived: usize,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn chats_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("chats")
}

fn chat_to_list_entry(chat: &ChatSession) -> ChatListEntry {
    ChatListEntry {
        id: chat.id.clone(),
        name: chat.name.clone(),
        scope_display: chat.scope.display_name(),
        status: chat.status,
        last_active_at: chat.last_active_at,
        message_count: chat.message_count,
        preview: chat.preview.clone(),
        stack_depth: chat.tracker.focus_stack.depth(),
        backlog_count: chat.tracker.backlog.len(),
    }
}

fn scope_hash(scope: &MemoryScope) -> String {
    let key = serde_json::to_string(scope).unwrap_or_default();
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in key.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
