//! `ff-sessions` — ForgeFleet session management.
//!
//! This crate provides:
//! - **session** — Session lifecycle: create, resume, end, timeout (DashMap-backed)
//! - **subagent** — Sub-agent spawning, tracking, steering, result collection
//! - **approval** — Exec approval system: security modes, ask modes, allowlists
//! - **workspace** — Workspace scoping and isolation per session
//! - **history** — Message history with pagination, search, export, compaction
//! - **context** — Context window tracking, budget management, compaction triggers

pub mod approval;
pub mod context;
pub mod history;
pub mod session;
pub mod subagent;
pub mod workspace;

// Re-export primary types at crate root for ergonomic imports.
pub use approval::{Approval, ApprovalManager, AskMode, SecurityMode};
pub use context::{ContextBudget, ContextManager, ContextStats};
pub use history::{HistoryStore, MessageEntry, MessageRole, SearchHit};
pub use session::{Session, SessionState, SessionStore};
pub use subagent::{SubAgent, SubAgentManager, SubAgentStatus};
pub use workspace::{WorkspaceConfig, WorkspaceManager};
