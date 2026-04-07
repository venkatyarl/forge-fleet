//! Workspace scoping and isolation.
//!
//! Each session can be scoped to a specific project/workspace directory.
//! This module enforces path isolation and tracks session ↔ workspace mappings.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Workspace configuration attached to a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Root directory for this session.
    pub root: PathBuf,

    /// Optional workspace display name.
    pub name: Option<String>,

    /// If true, session should not perform file mutations.
    pub read_only: bool,

    /// Optional contextual metadata (project id, repo, branch hints, etc.).
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl WorkspaceConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            name: None,
            read_only: false,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Return normalized (best-effort canonical) root path.
    pub fn normalized_root(&self) -> PathBuf {
        normalize_path(&self.root)
    }
}

/// Internal mapping entry.
#[derive(Debug, Clone)]
struct WorkspaceBinding {
    config: WorkspaceConfig,
    bound_at: DateTime<Utc>,
}

/// Manager for workspace scoping and isolation.
#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    session_workspaces: Arc<DashMap<Uuid, WorkspaceBinding>>,
    workspace_sessions: Arc<DashMap<String, Vec<Uuid>>>,
}

impl WorkspaceManager {
    pub fn new() -> Self {
        Self {
            session_workspaces: Arc::new(DashMap::new()),
            workspace_sessions: Arc::new(DashMap::new()),
        }
    }

    /// Bind a workspace to a session.
    pub fn bind(
        &self,
        session_id: Uuid,
        mut config: WorkspaceConfig,
    ) -> anyhow::Result<WorkspaceConfig> {
        // Normalize root path for stable isolation checks.
        config.root = normalize_path(&config.root);

        if !config.root.exists() {
            anyhow::bail!("workspace root does not exist: {}", config.root.display());
        }
        if !config.root.is_dir() {
            anyhow::bail!(
                "workspace root is not a directory: {}",
                config.root.display()
            );
        }

        // Remove old mapping if present.
        if let Some((_, old)) = self.session_workspaces.remove(&session_id) {
            let old_key = Self::workspace_key(&old.config.root);
            self.remove_session_from_workspace_index(&old_key, session_id);
        }

        let key = Self::workspace_key(&config.root);
        self.workspace_sessions
            .entry(key)
            .or_default()
            .push(session_id);

        self.session_workspaces.insert(
            session_id,
            WorkspaceBinding {
                config: config.clone(),
                bound_at: Utc::now(),
            },
        );

        info!(session_id = %session_id, root = %config.root.display(), "workspace bound to session");
        Ok(config)
    }

    /// Unbind a workspace from a session.
    pub fn unbind(&self, session_id: Uuid) -> bool {
        if let Some((_, binding)) = self.session_workspaces.remove(&session_id) {
            let key = Self::workspace_key(&binding.config.root);
            self.remove_session_from_workspace_index(&key, session_id);
            info!(session_id = %session_id, "workspace unbound from session");
            true
        } else {
            false
        }
    }

    /// Get workspace config for a session.
    pub fn get(&self, session_id: Uuid) -> Option<WorkspaceConfig> {
        self.session_workspaces
            .get(&session_id)
            .map(|b| b.config.clone())
    }

    /// Get bind time for a session.
    pub fn bound_at(&self, session_id: Uuid) -> Option<DateTime<Utc>> {
        self.session_workspaces.get(&session_id).map(|b| b.bound_at)
    }

    /// Resolve a path for a session while enforcing workspace isolation.
    ///
    /// Returns `None` if:
    /// - Session has no bound workspace
    /// - Path escapes the workspace root
    pub fn resolve_path(&self, session_id: Uuid, requested: impl AsRef<Path>) -> Option<PathBuf> {
        let binding = self.session_workspaces.get(&session_id)?;
        let root = &binding.config.root;

        let requested = requested.as_ref();
        let candidate = if requested.is_absolute() {
            normalize_path(requested)
        } else {
            normalize_path(root.join(requested))
        };

        if candidate.starts_with(root) {
            Some(candidate)
        } else {
            warn!(
                session_id = %session_id,
                requested = %requested.display(),
                root = %root.display(),
                "blocked path escaping workspace"
            );
            None
        }
    }

    /// Check if a requested path is allowed for a session.
    pub fn is_allowed(&self, session_id: Uuid, requested: impl AsRef<Path>) -> bool {
        self.resolve_path(session_id, requested).is_some()
    }

    /// Check whether a session is read-only.
    pub fn is_read_only(&self, session_id: Uuid) -> bool {
        self.session_workspaces
            .get(&session_id)
            .map(|b| b.config.read_only)
            .unwrap_or(false)
    }

    /// List all session IDs in a workspace.
    pub fn sessions_in_workspace(&self, root: impl AsRef<Path>) -> Vec<Uuid> {
        let key = Self::workspace_key(root.as_ref());
        self.workspace_sessions
            .get(&key)
            .map(|r| r.value().clone())
            .unwrap_or_default()
    }

    /// Return whether two sessions share the same workspace root.
    pub fn same_workspace(&self, a: Uuid, b: Uuid) -> bool {
        let wa = self.get(a);
        let wb = self.get(b);
        match (wa, wb) {
            (Some(wa), Some(wb)) => wa.normalized_root() == wb.normalized_root(),
            _ => false,
        }
    }

    /// Count currently bound sessions.
    pub fn len(&self) -> usize {
        self.session_workspaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.session_workspaces.is_empty()
    }

    fn workspace_key(path: &Path) -> String {
        normalize_path(path).to_string_lossy().to_string()
    }

    fn remove_session_from_workspace_index(&self, key: &str, session_id: Uuid) {
        if let Some(mut sessions) = self.workspace_sessions.get_mut(key) {
            sessions.retain(|id| *id != session_id);
            if sessions.is_empty() {
                drop(sessions);
                self.workspace_sessions.remove(key);
                debug!(workspace = %key, "removed empty workspace index");
            }
        }
    }
}

impl Default for WorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort path normalization without requiring filesystem existence.
fn normalize_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }

    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_and_resolve() {
        let mgr = WorkspaceManager::new();
        let sid = Uuid::new_v4();
        let cwd = std::env::current_dir().unwrap();

        mgr.bind(sid, WorkspaceConfig::new(&cwd)).unwrap();
        let p = mgr.resolve_path(sid, "src/lib.rs").unwrap();
        assert!(p.starts_with(&cwd));
    }

    #[test]
    fn blocks_escape() {
        let mgr = WorkspaceManager::new();
        let sid = Uuid::new_v4();
        let cwd = std::env::current_dir().unwrap();

        mgr.bind(sid, WorkspaceConfig::new(&cwd)).unwrap();
        let escaped = mgr.resolve_path(sid, "../../etc/passwd");
        assert!(escaped.is_none());
    }

    #[test]
    fn same_workspace_check() {
        let mgr = WorkspaceManager::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let cwd = std::env::current_dir().unwrap();

        mgr.bind(a, WorkspaceConfig::new(&cwd)).unwrap();
        mgr.bind(b, WorkspaceConfig::new(&cwd)).unwrap();

        assert!(mgr.same_workspace(a, b));
    }
}
