//! Session lifecycle management.
//!
//! Each session represents an ongoing conversation between a user on a channel
//! and the fleet. Sessions are stored in a concurrent [`DashMap`] for lock-free
//! reads and fine-grained write locks.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::workspace::WorkspaceConfig;

// ─── Session State ───────────────────────────────────────────────────────────

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Session is active and accepting messages.
    Active,
    /// Session is paused (e.g. waiting for approval).
    Paused,
    /// Session ended normally.
    Ended,
    /// Session timed out due to inactivity.
    TimedOut,
    /// Session encountered an unrecoverable error.
    Error,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Paused => write!(f, "paused"),
            Self::Ended => write!(f, "ended"),
            Self::TimedOut => write!(f, "timed_out"),
            Self::Error => write!(f, "error"),
        }
    }
}

// ─── Session ─────────────────────────────────────────────────────────────────

/// A single conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session identifier.
    pub id: Uuid,

    /// Channel this session lives on (e.g. "telegram", "discord", "web").
    pub channel: String,

    /// User identifier within the channel.
    pub user: String,

    /// Model currently assigned to this session (e.g. "claude-opus-4-0520", "qwen-32b").
    pub model: String,

    /// Current lifecycle state.
    pub state: SessionState,

    /// Optional label for display / debugging.
    pub label: Option<String>,

    /// Parent session ID (if this is a sub-agent session).
    pub parent_id: Option<Uuid>,

    /// Workspace configuration for this session.
    pub workspace: Option<WorkspaceConfig>,

    /// When the session was created.
    pub created_at: DateTime<Utc>,

    /// Last activity timestamp (updated on every message).
    pub last_active: DateTime<Utc>,

    /// Total messages exchanged in this session.
    pub message_count: u64,
}

impl Session {
    /// Create a new active session.
    pub fn new(
        channel: impl Into<String>,
        user: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            channel: channel.into(),
            user: user.into(),
            model: model.into(),
            state: SessionState::Active,
            label: None,
            parent_id: None,
            workspace: None,
            created_at: now,
            last_active: now,
            message_count: 0,
        }
    }

    /// Create a child session (sub-agent) tied to a parent.
    pub fn new_child(
        parent_id: Uuid,
        channel: impl Into<String>,
        user: impl Into<String>,
        model: impl Into<String>,
        label: Option<String>,
    ) -> Self {
        let mut session = Self::new(channel, user, model);
        session.parent_id = Some(parent_id);
        session.label = label;
        session
    }

    /// Touch the session — update `last_active` and bump message count.
    pub fn touch(&mut self) {
        self.last_active = Utc::now();
        self.message_count += 1;
    }

    /// Check whether this session has been inactive longer than `timeout`.
    pub fn is_timed_out(&self, timeout: Duration) -> bool {
        let elapsed = Utc::now()
            .signed_duration_since(self.last_active)
            .to_std()
            .unwrap_or(Duration::ZERO);
        elapsed > timeout
    }

    /// Whether this session is still accepting messages.
    pub fn is_active(&self) -> bool {
        self.state == SessionState::Active
    }

    /// Whether this session is a sub-agent child.
    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }
}

// ─── Session Store ───────────────────────────────────────────────────────────

/// Thread-safe, concurrent session store backed by [`DashMap`].
///
/// This is the primary entry point for session lifecycle management.
/// All operations are lock-free for reads and use fine-grained locks for writes.
#[derive(Debug, Clone)]
pub struct SessionStore {
    /// Active sessions keyed by session ID.
    sessions: Arc<DashMap<Uuid, Session>>,

    /// Index: channel:user → session ID for fast lookup.
    channel_index: Arc<DashMap<String, Uuid>>,

    /// Inactivity timeout (sessions auto-expire after this duration).
    timeout: Duration,
}

impl SessionStore {
    /// Create a new store with the given inactivity timeout.
    pub fn new(timeout: Duration) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            channel_index: Arc::new(DashMap::new()),
            timeout,
        }
    }

    /// Create a new session and register it.
    pub fn create(
        &self,
        channel: impl Into<String>,
        user: impl Into<String>,
        model: impl Into<String>,
    ) -> Session {
        let session = Session::new(channel, user, model);
        let key = Self::channel_key(&session.channel, &session.user);
        info!(session_id = %session.id, channel = %session.channel, user = %session.user, "session created");

        self.channel_index.insert(key, session.id);
        self.sessions.insert(session.id, session.clone());
        session
    }

    /// Resume an existing session by ID. Returns `None` if not found or expired.
    ///
    /// If the session has timed out, it is marked as `TimedOut` and `None` is returned.
    pub fn resume(&self, session_id: Uuid) -> Option<Session> {
        let mut entry = self.sessions.get_mut(&session_id)?;
        let session = entry.value_mut();

        if session.is_timed_out(self.timeout) {
            warn!(session_id = %session_id, "session timed out on resume attempt");
            session.state = SessionState::TimedOut;
            return None;
        }

        if session.state != SessionState::Active && session.state != SessionState::Paused {
            debug!(session_id = %session_id, state = %session.state, "cannot resume session in terminal state");
            return None;
        }

        session.state = SessionState::Active;
        session.touch();
        info!(session_id = %session_id, "session resumed");
        Some(session.clone())
    }

    /// Look up a session by channel + user. Creates a new one if none exists.
    pub fn get_or_create(
        &self,
        channel: impl Into<String>,
        user: impl Into<String>,
        model: impl Into<String>,
    ) -> Session {
        let channel = channel.into();
        let user = user.into();
        let key = Self::channel_key(&channel, &user);

        // Try to find existing active session for this channel:user.
        if let Some(session_id) = self.channel_index.get(&key) {
            if let Some(session) = self.resume(*session_id) {
                return session;
            }
            // Expired or terminal — remove stale index entry.
            self.channel_index.remove(&key);
        }

        self.create(channel, user, model)
    }

    /// End a session gracefully.
    pub fn end(&self, session_id: Uuid) -> Option<Session> {
        let mut entry = self.sessions.get_mut(&session_id)?;
        let session = entry.value_mut();
        session.state = SessionState::Ended;
        info!(session_id = %session_id, "session ended");

        // Clean up channel index.
        let key = Self::channel_key(&session.channel, &session.user);
        self.channel_index.remove(&key);

        Some(session.clone())
    }

    /// Get a session by ID (read-only snapshot).
    pub fn get(&self, session_id: Uuid) -> Option<Session> {
        self.sessions.get(&session_id).map(|r| r.value().clone())
    }

    /// Touch a session (update last_active, bump message count).
    pub fn touch(&self, session_id: Uuid) -> bool {
        if let Some(mut entry) = self.sessions.get_mut(&session_id) {
            entry.value_mut().touch();
            true
        } else {
            false
        }
    }

    /// List all sessions matching an optional state filter.
    pub fn list(&self, state_filter: Option<SessionState>) -> Vec<Session> {
        self.sessions
            .iter()
            .filter(|entry| {
                state_filter
                    .as_ref()
                    .is_none_or(|f| &entry.value().state == f)
            })
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// List all child sessions of a given parent.
    pub fn children_of(&self, parent_id: Uuid) -> Vec<Session> {
        self.sessions
            .iter()
            .filter(|entry| entry.value().parent_id == Some(parent_id))
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Sweep timed-out sessions. Returns the number of sessions expired.
    pub fn sweep_timed_out(&self) -> usize {
        let mut expired = Vec::new();

        for entry in self.sessions.iter() {
            let session = entry.value();
            if session.state == SessionState::Active && session.is_timed_out(self.timeout) {
                expired.push(*entry.key());
            }
        }

        for id in &expired {
            if let Some(mut entry) = self.sessions.get_mut(id) {
                let session = entry.value_mut();
                session.state = SessionState::TimedOut;
                let key = Self::channel_key(&session.channel, &session.user);
                self.channel_index.remove(&key);
                info!(session_id = %id, "session timed out during sweep");
            }
        }

        expired.len()
    }

    /// Spawn a background task that periodically sweeps timed-out sessions.
    pub fn spawn_sweeper(self, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let count = self.sweep_timed_out();
                if count > 0 {
                    info!(expired = count, "session sweep complete");
                }
            }
        })
    }

    /// Total number of sessions in the store (all states).
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|e| e.value().state == SessionState::Active)
            .count()
    }

    /// Build a channel index key.
    fn channel_key(channel: &str, user: &str) -> String {
        format!("{channel}:{user}")
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_resume() {
        let store = SessionStore::new(Duration::from_secs(3600));
        let session = store.create("telegram", "user123", "claude-opus-4-0520");
        assert_eq!(session.state, SessionState::Active);
        assert_eq!(session.channel, "telegram");

        let resumed = store.resume(session.id).expect("should resume");
        assert_eq!(resumed.id, session.id);
        assert!(resumed.message_count >= 1); // touch bumps count
    }

    #[test]
    fn end_session() {
        let store = SessionStore::new(Duration::from_secs(3600));
        let session = store.create("discord", "user456", "qwen-32b");
        let ended = store.end(session.id).expect("should end");
        assert_eq!(ended.state, SessionState::Ended);

        // Cannot resume an ended session.
        assert!(store.resume(session.id).is_none());
    }

    #[test]
    fn get_or_create_reuses_active() {
        let store = SessionStore::new(Duration::from_secs(3600));
        let s1 = store.get_or_create("telegram", "user1", "model-a");
        let s2 = store.get_or_create("telegram", "user1", "model-a");
        assert_eq!(s1.id, s2.id);
    }

    #[test]
    fn child_session() {
        let store = SessionStore::new(Duration::from_secs(3600));
        let parent = store.create("telegram", "user1", "claude-opus-4-0520");
        let child = Session::new_child(
            parent.id,
            "subagent",
            "user1",
            "qwen-9b",
            Some("researcher".into()),
        );
        store.sessions.insert(child.id, child.clone());

        let children = store.children_of(parent.id);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].parent_id, Some(parent.id));
    }

    #[test]
    fn sweep_timeout() {
        let store = SessionStore::new(Duration::from_secs(0)); // immediate timeout
        let session = store.create("web", "user1", "model-a");

        // Session was just created, but timeout is 0 seconds — it should expire on sweep.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let expired = store.sweep_timed_out();
        assert_eq!(expired, 1);

        let s = store.get(session.id).unwrap();
        assert_eq!(s.state, SessionState::TimedOut);
    }
}
