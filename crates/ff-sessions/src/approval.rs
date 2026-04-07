//! Exec approval system.
//!
//! Provides per-session approval policies with:
//! - Security modes (`full`, `allowlist`, `deny`)
//! - Ask modes (`off`, `on_miss`, `always`)
//! - Command allowlist management
//! - Pending approval queue and decision tracking

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Security posture for command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SecurityMode {
    /// Allow commands by default.
    Full,
    /// Allow only commands matching the session allowlist.
    #[default]
    Allowlist,
    /// Deny all command execution.
    Deny,
}

/// Ask mode for human approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AskMode {
    /// Never ask; auto-allow/deny based on security mode.
    Off,
    /// Ask only when command misses allowlist.
    #[default]
    OnMiss,
    /// Ask for every command.
    Always,
}

/// State of an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
}

/// A single approval request for a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: Uuid,
    pub session_id: Uuid,
    pub command: String,
    pub state: ApprovalState,
    pub reason: String,
    pub requested_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

impl Approval {
    fn pending(session_id: Uuid, command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            command: command.into(),
            state: ApprovalState::Pending,
            reason: reason.into(),
            requested_at: Utc::now(),
            decided_at: None,
        }
    }

    fn approve(&mut self) {
        self.state = ApprovalState::Approved;
        self.decided_at = Some(Utc::now());
    }

    fn deny(&mut self) {
        self.state = ApprovalState::Denied;
        self.decided_at = Some(Utc::now());
    }
}

/// Per-session approval policy and counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    pub security_mode: SecurityMode,
    pub ask_mode: AskMode,
    pub allowlist: Vec<String>,
    pub total_checked: u64,
    pub auto_allowed: u64,
    pub auto_denied: u64,
    pub pending_requests: u64,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            security_mode: SecurityMode::Allowlist,
            ask_mode: AskMode::OnMiss,
            allowlist: Vec::new(),
            total_checked: 0,
            auto_allowed: 0,
            auto_denied: 0,
            pending_requests: 0,
        }
    }
}

/// Result of evaluating a command against a policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub allowed: bool,
    pub needs_approval: bool,
    pub approval_id: Option<Uuid>,
    pub reason: String,
}

impl ApprovalDecision {
    fn allow(reason: impl Into<String>) -> Self {
        Self {
            allowed: true,
            needs_approval: false,
            approval_id: None,
            reason: reason.into(),
        }
    }

    fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            needs_approval: false,
            approval_id: None,
            reason: reason.into(),
        }
    }

    fn pending(approval_id: Uuid, reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            needs_approval: true,
            approval_id: Some(approval_id),
            reason: reason.into(),
        }
    }
}

/// Main manager for command approvals.
#[derive(Debug, Clone)]
pub struct ApprovalManager {
    policies: Arc<DashMap<Uuid, ApprovalPolicy>>,
    requests: Arc<DashMap<Uuid, Approval>>,
}

impl ApprovalManager {
    pub fn new() -> Self {
        Self {
            policies: Arc::new(DashMap::new()),
            requests: Arc::new(DashMap::new()),
        }
    }

    /// Ensure a session policy exists and return it.
    pub fn ensure_session(&self, session_id: Uuid) -> ApprovalPolicy {
        if let Some(policy) = self.policies.get(&session_id) {
            return policy.value().clone();
        }

        let policy = ApprovalPolicy::default();
        self.policies.insert(session_id, policy.clone());
        policy
    }

    /// Set security mode for a session.
    pub fn set_security_mode(&self, session_id: Uuid, mode: SecurityMode) {
        self.policies.entry(session_id).or_default().security_mode = mode;
    }

    /// Set ask mode for a session.
    pub fn set_ask_mode(&self, session_id: Uuid, mode: AskMode) {
        self.policies.entry(session_id).or_default().ask_mode = mode;
    }

    /// Add an allowlist entry for a session.
    ///
    /// Pattern syntax:
    /// - `git status` exact match
    /// - `git *` prefix match
    /// - `* --help` suffix match
    /// - `python * -m pytest` wildcard middle (single `*` supported)
    pub fn allowlist_add(&self, session_id: Uuid, pattern: impl Into<String>) {
        let pattern = pattern.into();
        let mut policy = self.policies.entry(session_id).or_default();
        if !policy.allowlist.iter().any(|p| p == &pattern) {
            policy.allowlist.push(pattern);
        }
    }

    /// Remove an allowlist entry for a session.
    pub fn allowlist_remove(&self, session_id: Uuid, pattern: &str) -> bool {
        if let Some(mut policy) = self.policies.get_mut(&session_id) {
            let before = policy.allowlist.len();
            policy.allowlist.retain(|p| p != pattern);
            return policy.allowlist.len() != before;
        }
        false
    }

    /// Clear all allowlist entries for a session.
    pub fn allowlist_clear(&self, session_id: Uuid) {
        self.policies
            .entry(session_id)
            .or_default()
            .allowlist
            .clear();
    }

    /// Get current policy for a session.
    pub fn policy(&self, session_id: Uuid) -> ApprovalPolicy {
        self.ensure_session(session_id)
    }

    /// Evaluate a command for execution.
    pub fn evaluate(&self, session_id: Uuid, command: &str) -> ApprovalDecision {
        let mut policy = self.policies.entry(session_id).or_default();
        policy.total_checked += 1;

        let allowlisted = self.matches_allowlist(&policy.allowlist, command);
        let decision = match (policy.security_mode, policy.ask_mode, allowlisted) {
            (SecurityMode::Deny, _, _) => {
                policy.auto_denied += 1;
                ApprovalDecision::deny("security_mode=deny blocks all commands")
            }

            (SecurityMode::Full, AskMode::Off, _) => {
                policy.auto_allowed += 1;
                ApprovalDecision::allow("security_mode=full with ask_mode=off")
            }
            (SecurityMode::Full, AskMode::OnMiss, _) => {
                policy.auto_allowed += 1;
                ApprovalDecision::allow("security_mode=full allows command")
            }
            (SecurityMode::Full, AskMode::Always, _) => {
                self.create_pending(session_id, command, "ask_mode=always")
            }

            (SecurityMode::Allowlist, AskMode::Always, _) => {
                self.create_pending(session_id, command, "ask_mode=always")
            }
            (SecurityMode::Allowlist, AskMode::Off, true)
            | (SecurityMode::Allowlist, AskMode::OnMiss, true) => {
                policy.auto_allowed += 1;
                ApprovalDecision::allow("command matched allowlist")
            }
            (SecurityMode::Allowlist, AskMode::Off, false) => {
                policy.auto_denied += 1;
                ApprovalDecision::deny("command not in allowlist")
            }
            (SecurityMode::Allowlist, AskMode::OnMiss, false) => {
                self.create_pending(session_id, command, "command missed allowlist")
            }
        };

        if decision.needs_approval {
            policy.pending_requests += 1;
        }

        debug!(
            session_id = %session_id,
            allowed = decision.allowed,
            needs_approval = decision.needs_approval,
            reason = %decision.reason,
            "approval decision"
        );

        decision
    }

    /// Approve a pending approval request.
    pub fn approve(&self, approval_id: Uuid) -> bool {
        if let Some(mut req) = self.requests.get_mut(&approval_id)
            && req.state == ApprovalState::Pending
        {
            req.approve();
            info!(approval_id = %approval_id, session_id = %req.session_id, "approval granted");
            return true;
        }
        false
    }

    /// Deny a pending approval request.
    pub fn deny(&self, approval_id: Uuid) -> bool {
        if let Some(mut req) = self.requests.get_mut(&approval_id)
            && req.state == ApprovalState::Pending
        {
            req.deny();
            warn!(approval_id = %approval_id, session_id = %req.session_id, "approval denied");
            return true;
        }
        false
    }

    /// Get approval request by ID.
    pub fn get_request(&self, approval_id: Uuid) -> Option<Approval> {
        self.requests.get(&approval_id).map(|r| r.value().clone())
    }

    /// List pending requests for a session.
    pub fn pending_for_session(&self, session_id: Uuid) -> Vec<Approval> {
        self.requests
            .iter()
            .filter(|r| {
                let req = r.value();
                req.session_id == session_id && req.state == ApprovalState::Pending
            })
            .map(|r| r.value().clone())
            .collect()
    }

    /// Check whether a request is approved.
    pub fn is_approved(&self, approval_id: Uuid) -> bool {
        self.requests
            .get(&approval_id)
            .map(|r| r.state == ApprovalState::Approved)
            .unwrap_or(false)
    }

    /// Cleanup old completed requests for a session.
    pub fn cleanup_completed(&self, session_id: Uuid, max_age: chrono::Duration) -> usize {
        let now = Utc::now();
        let to_remove: Vec<Uuid> = self
            .requests
            .iter()
            .filter_map(|r| {
                let req = r.value();
                let done = req.state != ApprovalState::Pending;
                let old_enough = req
                    .decided_at
                    .map(|t| now.signed_duration_since(t) >= max_age)
                    .unwrap_or(false);
                if req.session_id == session_id && done && old_enough {
                    Some(req.id)
                } else {
                    None
                }
            })
            .collect();

        let count = to_remove.len();
        for id in to_remove {
            self.requests.remove(&id);
        }
        count
    }

    fn create_pending(&self, session_id: Uuid, command: &str, reason: &str) -> ApprovalDecision {
        let req = Approval::pending(session_id, command, reason);
        let id = req.id;
        self.requests.insert(id, req);
        ApprovalDecision::pending(id, reason)
    }

    fn matches_allowlist(&self, allowlist: &[String], command: &str) -> bool {
        allowlist
            .iter()
            .any(|pattern| Self::pattern_match(pattern, command))
    }

    fn pattern_match(pattern: &str, value: &str) -> bool {
        if pattern == "*" {
            return true;
        }

        if !pattern.contains('*') {
            return pattern.trim() == value.trim();
        }

        let parts: Vec<&str> = pattern.split('*').collect();
        match parts.as_slice() {
            [prefix, suffix] => {
                if prefix.is_empty() {
                    value.ends_with(suffix)
                } else if suffix.is_empty() {
                    value.starts_with(prefix)
                } else {
                    value.starts_with(prefix) && value.ends_with(suffix)
                }
            }
            _ => {
                // Multi-* wildcard fallback: ensure each non-empty part appears in order.
                let mut cursor = 0usize;
                for part in parts.iter().filter(|p| !p.is_empty()) {
                    if let Some(pos) = value[cursor..].find(part) {
                        cursor += pos + part.len();
                    } else {
                        return false;
                    }
                }
                true
            }
        }
    }
}

impl Default for ApprovalManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_on_miss_requires_approval() {
        let mgr = ApprovalManager::new();
        let sid = Uuid::new_v4();
        mgr.set_security_mode(sid, SecurityMode::Allowlist);
        mgr.set_ask_mode(sid, AskMode::OnMiss);
        mgr.allowlist_add(sid, "git status");

        let d1 = mgr.evaluate(sid, "git status");
        assert!(d1.allowed);
        assert!(!d1.needs_approval);

        let d2 = mgr.evaluate(sid, "rm -rf /");
        assert!(!d2.allowed);
        assert!(d2.needs_approval);
        assert!(d2.approval_id.is_some());
    }

    #[test]
    fn deny_mode_blocks_all() {
        let mgr = ApprovalManager::new();
        let sid = Uuid::new_v4();
        mgr.set_security_mode(sid, SecurityMode::Deny);

        let d = mgr.evaluate(sid, "echo hello");
        assert!(!d.allowed);
        assert!(!d.needs_approval);
    }

    #[test]
    fn approval_request_flow() {
        let mgr = ApprovalManager::new();
        let sid = Uuid::new_v4();
        mgr.set_security_mode(sid, SecurityMode::Full);
        mgr.set_ask_mode(sid, AskMode::Always);

        let d = mgr.evaluate(sid, "echo hello");
        let approval_id = d.approval_id.expect("expected pending approval");
        assert!(mgr.get_request(approval_id).is_some());
        assert!(mgr.approve(approval_id));
        assert!(mgr.is_approved(approval_id));
    }

    #[test]
    fn wildcard_patterns() {
        assert!(ApprovalManager::pattern_match("git *", "git status"));
        assert!(ApprovalManager::pattern_match("* --help", "cargo --help"));
        assert!(ApprovalManager::pattern_match(
            "python * -m pytest",
            "python -q -m pytest"
        ));
        assert!(!ApprovalManager::pattern_match("git *", "cargo test"));
    }
}
