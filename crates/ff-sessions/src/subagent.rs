//! Sub-agent spawning, tracking, steering, and result collection.
//!
//! Sub-agents are child sessions that run parallel work on behalf of a parent
//! session. This module manages the parent → child relationship, tracks status,
//! and collects results.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

// ─── Sub-Agent Status ────────────────────────────────────────────────────────

/// Lifecycle status of a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentStatus {
    /// Queued but not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Completed,
    /// Failed with an error.
    Failed,
    /// Killed by the parent or system.
    Killed,
    /// Timed out.
    TimedOut,
}

impl std::fmt::Display for SubAgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Killed => write!(f, "killed"),
            Self::TimedOut => write!(f, "timed_out"),
        }
    }
}

impl SubAgentStatus {
    /// Whether this status is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Killed | Self::TimedOut
        )
    }
}

// ─── Sub-Agent ───────────────────────────────────────────────────────────────

/// A sub-agent spawned by a parent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgent {
    /// Unique sub-agent ID (also the child session ID).
    pub id: Uuid,

    /// Parent session ID that spawned this sub-agent.
    pub parent_id: Uuid,

    /// Human-readable label (e.g. "context-engineer", "code-writer").
    pub label: String,

    /// Task description assigned to this sub-agent.
    pub task: String,

    /// Model assigned to run this sub-agent.
    pub model: String,

    /// Current status.
    pub status: SubAgentStatus,

    /// Result payload (populated on completion).
    pub result: Option<String>,

    /// Error message (populated on failure).
    pub error: Option<String>,

    /// Steering messages sent to this sub-agent.
    pub steered: Vec<SteerMessage>,

    /// When the sub-agent was spawned.
    pub created_at: DateTime<Utc>,

    /// Last status update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// A steering message sent to a running sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerMessage {
    /// The steering instruction.
    pub message: String,
    /// When it was sent.
    pub sent_at: DateTime<Utc>,
}

impl SubAgent {
    /// Create a new pending sub-agent.
    pub fn new(
        parent_id: Uuid,
        label: impl Into<String>,
        task: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            parent_id,
            label: label.into(),
            task: task.into(),
            model: model.into(),
            status: SubAgentStatus::Pending,
            result: None,
            error: None,
            steered: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Transition to running.
    pub fn mark_running(&mut self) {
        self.status = SubAgentStatus::Running;
        self.updated_at = Utc::now();
    }

    /// Mark as completed with a result.
    pub fn mark_completed(&mut self, result: impl Into<String>) {
        self.status = SubAgentStatus::Completed;
        self.result = Some(result.into());
        self.updated_at = Utc::now();
    }

    /// Mark as failed with an error.
    pub fn mark_failed(&mut self, error: impl Into<String>) {
        self.status = SubAgentStatus::Failed;
        self.error = Some(error.into());
        self.updated_at = Utc::now();
    }

    /// Mark as killed.
    pub fn mark_killed(&mut self) {
        self.status = SubAgentStatus::Killed;
        self.updated_at = Utc::now();
    }

    /// Mark as timed out.
    pub fn mark_timed_out(&mut self) {
        self.status = SubAgentStatus::TimedOut;
        self.updated_at = Utc::now();
    }

    /// Add a steering message.
    pub fn steer(&mut self, message: impl Into<String>) {
        self.steered.push(SteerMessage {
            message: message.into(),
            sent_at: Utc::now(),
        });
        self.updated_at = Utc::now();
    }

    /// Duration since creation.
    pub fn elapsed(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.created_at)
    }
}

// ─── Sub-Agent Manager ───────────────────────────────────────────────────────

/// Manages all sub-agents across all sessions.
///
/// Thread-safe via [`DashMap`]. Supports spawn, track, steer, kill, and result
/// collection.
#[derive(Debug, Clone)]
pub struct SubAgentManager {
    /// All sub-agents keyed by their ID.
    agents: Arc<DashMap<Uuid, SubAgent>>,

    /// Index: parent_id → list of child IDs.
    parent_index: Arc<DashMap<Uuid, Vec<Uuid>>>,
}

impl SubAgentManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        Self {
            agents: Arc::new(DashMap::new()),
            parent_index: Arc::new(DashMap::new()),
        }
    }

    /// Spawn (register) a new sub-agent. Returns the sub-agent with its assigned ID.
    pub fn spawn(
        &self,
        parent_id: Uuid,
        label: impl Into<String>,
        task: impl Into<String>,
        model: impl Into<String>,
    ) -> SubAgent {
        let agent = SubAgent::new(parent_id, label, task, model);
        info!(
            agent_id = %agent.id,
            parent_id = %parent_id,
            label = %agent.label,
            "sub-agent spawned"
        );

        self.agents.insert(agent.id, agent.clone());
        self.parent_index
            .entry(parent_id)
            .or_default()
            .push(agent.id);

        agent
    }

    /// Get a sub-agent by ID.
    pub fn get(&self, agent_id: Uuid) -> Option<SubAgent> {
        self.agents.get(&agent_id).map(|r| r.value().clone())
    }

    /// Mark a sub-agent as running.
    pub fn mark_running(&self, agent_id: Uuid) -> bool {
        self.update(agent_id, |a| a.mark_running())
    }

    /// Mark a sub-agent as completed with a result.
    pub fn complete(&self, agent_id: Uuid, result: impl Into<String>) -> bool {
        let result = result.into();
        self.update(agent_id, |a| a.mark_completed(result.clone()))
    }

    /// Mark a sub-agent as failed.
    pub fn fail(&self, agent_id: Uuid, error: impl Into<String>) -> bool {
        let error = error.into();
        self.update(agent_id, |a| a.mark_failed(error.clone()))
    }

    /// Kill a sub-agent.
    pub fn kill(&self, agent_id: Uuid) -> bool {
        if self.update(agent_id, |a| a.mark_killed()) {
            warn!(agent_id = %agent_id, "sub-agent killed");
            true
        } else {
            false
        }
    }

    /// Send a steering message to a running sub-agent.
    pub fn steer(&self, agent_id: Uuid, message: impl Into<String>) -> bool {
        let message = message.into();
        if self.update(agent_id, |a| a.steer(message.clone())) {
            debug!(agent_id = %agent_id, "sub-agent steered");
            true
        } else {
            false
        }
    }

    /// List all sub-agents for a parent session.
    pub fn children_of(&self, parent_id: Uuid) -> Vec<SubAgent> {
        let ids = self
            .parent_index
            .get(&parent_id)
            .map(|r| r.value().clone())
            .unwrap_or_default();

        ids.iter()
            .filter_map(|id| self.agents.get(id).map(|r| r.value().clone()))
            .collect()
    }

    /// Collect results from all completed sub-agents of a parent.
    pub fn collect_results(&self, parent_id: Uuid) -> Vec<(String, String)> {
        self.children_of(parent_id)
            .into_iter()
            .filter(|a| a.status == SubAgentStatus::Completed)
            .filter_map(|a| a.result.map(|r| (a.label, r)))
            .collect()
    }

    /// Check if all sub-agents of a parent are in terminal state.
    pub fn all_done(&self, parent_id: Uuid) -> bool {
        let children = self.children_of(parent_id);
        if children.is_empty() {
            return true;
        }
        children.iter().all(|a| a.status.is_terminal())
    }

    /// Count sub-agents by status for a parent.
    pub fn status_counts(&self, parent_id: Uuid) -> SubAgentCounts {
        let children = self.children_of(parent_id);
        let mut counts = SubAgentCounts::default();
        for child in &children {
            match child.status {
                SubAgentStatus::Pending => counts.pending += 1,
                SubAgentStatus::Running => counts.running += 1,
                SubAgentStatus::Completed => counts.completed += 1,
                SubAgentStatus::Failed => counts.failed += 1,
                SubAgentStatus::Killed => counts.killed += 1,
                SubAgentStatus::TimedOut => counts.timed_out += 1,
            }
        }
        counts.total = children.len();
        counts
    }

    /// Kill all running sub-agents of a parent.
    pub fn kill_all(&self, parent_id: Uuid) -> usize {
        let children = self.children_of(parent_id);
        let mut killed = 0;
        for child in children {
            if !child.status.is_terminal() {
                self.kill(child.id);
                killed += 1;
            }
        }
        killed
    }

    /// Remove all sub-agents for a parent (cleanup after session end).
    pub fn cleanup(&self, parent_id: Uuid) {
        if let Some((_, ids)) = self.parent_index.remove(&parent_id) {
            for id in ids {
                self.agents.remove(&id);
            }
            debug!(parent_id = %parent_id, "sub-agents cleaned up");
        }
    }

    /// Total number of tracked sub-agents.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether there are no tracked sub-agents.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Internal helper: apply a mutation to a sub-agent by ID.
    fn update(&self, agent_id: Uuid, f: impl FnOnce(&mut SubAgent)) -> bool {
        if let Some(mut entry) = self.agents.get_mut(&agent_id) {
            f(entry.value_mut());
            true
        } else {
            false
        }
    }
}

impl Default for SubAgentManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Counts ──────────────────────────────────────────────────────────────────

/// Summary counts of sub-agent statuses for a parent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubAgentCounts {
    pub total: usize,
    pub pending: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub killed: usize,
    pub timed_out: usize,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_complete() {
        let mgr = SubAgentManager::new();
        let parent = Uuid::new_v4();
        let agent = mgr.spawn(parent, "researcher", "Find context", "qwen-9b");

        assert_eq!(agent.status, SubAgentStatus::Pending);
        assert!(!mgr.all_done(parent));

        mgr.mark_running(agent.id);
        let a = mgr.get(agent.id).unwrap();
        assert_eq!(a.status, SubAgentStatus::Running);

        mgr.complete(agent.id, "Found 42 relevant files");
        assert!(mgr.all_done(parent));

        let results = mgr.collect_results(parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "researcher");
    }

    #[test]
    fn steer_and_kill() {
        let mgr = SubAgentManager::new();
        let parent = Uuid::new_v4();
        let agent = mgr.spawn(parent, "writer", "Write code", "qwen-32b");

        mgr.mark_running(agent.id);
        mgr.steer(agent.id, "Focus on error handling");

        let a = mgr.get(agent.id).unwrap();
        assert_eq!(a.steered.len(), 1);

        mgr.kill(agent.id);
        let a = mgr.get(agent.id).unwrap();
        assert_eq!(a.status, SubAgentStatus::Killed);
    }

    #[test]
    fn kill_all_and_cleanup() {
        let mgr = SubAgentManager::new();
        let parent = Uuid::new_v4();
        mgr.spawn(parent, "a", "task-a", "model");
        mgr.spawn(parent, "b", "task-b", "model");
        mgr.spawn(parent, "c", "task-c", "model");

        assert_eq!(mgr.kill_all(parent), 3);
        assert!(mgr.all_done(parent));

        mgr.cleanup(parent);
        assert_eq!(mgr.children_of(parent).len(), 0);
    }

    #[test]
    fn status_counts() {
        let mgr = SubAgentManager::new();
        let parent = Uuid::new_v4();
        let a1 = mgr.spawn(parent, "a", "t", "m");
        let a2 = mgr.spawn(parent, "b", "t", "m");
        let _a3 = mgr.spawn(parent, "c", "t", "m");

        mgr.mark_running(a1.id);
        mgr.complete(a2.id, "done");

        let counts = mgr.status_counts(parent);
        assert_eq!(counts.total, 3);
        assert_eq!(counts.running, 1);
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.pending, 1);
    }
}
