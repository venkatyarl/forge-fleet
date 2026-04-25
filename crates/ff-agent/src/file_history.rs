//! File history — track every file change for undo, diff display, and IDE integration.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Global file history store keyed by session ID.
static HISTORY_STORE: std::sync::LazyLock<Arc<DashMap<String, FileHistory>>> =
    std::sync::LazyLock::new(|| Arc::new(DashMap::new()));

/// Get or create file history for a session.
pub fn session_file_history(_session_id: &str) -> Arc<DashMap<String, FileHistory>> {
    HISTORY_STORE.clone()
}

/// File history for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileHistory {
    /// Ordered list of changes.
    pub changes: Vec<FileChange>,
    /// Per-file access counts (for post-compact file recovery).
    pub access_counts: HashMap<String, u32>,
}

/// A single file change event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub change_type: ChangeType,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
    pub tool_name: String,
    pub turn: u32,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Read,
    Write,
    Edit,
    Create,
    Delete,
}

impl FileHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a file read access.
    pub fn record_read(&mut self, path: &str) {
        *self.access_counts.entry(path.to_string()).or_insert(0) += 1;
    }

    /// Record a file change (write or edit).
    pub fn record_change(
        &mut self,
        path: PathBuf,
        change_type: ChangeType,
        before: Option<String>,
        after: Option<String>,
        tool_name: &str,
        turn: u32,
    ) {
        let path_str = path.to_string_lossy().to_string();
        *self.access_counts.entry(path_str).or_insert(0) += 1;

        self.changes.push(FileChange {
            path,
            change_type,
            before_content: before,
            after_content: after,
            tool_name: tool_name.to_string(),
            turn,
            timestamp: Utc::now(),
        });
    }

    /// Get the N most accessed files (for post-compact recovery).
    pub fn top_accessed_files(&self, n: usize) -> Vec<(String, u32)> {
        let mut entries: Vec<_> = self
            .access_counts
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1));
        entries.truncate(n);
        entries
    }

    /// Get all changes for a specific file.
    pub fn changes_for_file(&self, path: &str) -> Vec<&FileChange> {
        self.changes
            .iter()
            .filter(|c| c.path.to_string_lossy() == path)
            .collect()
    }

    /// Get the last change (for undo).
    pub fn last_change(&self) -> Option<&FileChange> {
        self.changes.last()
    }

    /// Generate a unified diff for a change.
    pub fn diff_for_change(change: &FileChange) -> String {
        let before = change.before_content.as_deref().unwrap_or("");
        let after = change.after_content.as_deref().unwrap_or("");

        if before == after {
            return "(no changes)".into();
        }

        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();

        let mut diff = format!(
            "--- {}\n+++ {}\n",
            change.path.display(),
            change.path.display()
        );

        // Simple line-by-line diff
        let max_lines = before_lines.len().max(after_lines.len());
        for i in 0..max_lines {
            let b = before_lines.get(i).copied().unwrap_or("");
            let a = after_lines.get(i).copied().unwrap_or("");
            if b != a {
                if i < before_lines.len() {
                    diff.push_str(&format!("-{b}\n"));
                }
                if i < after_lines.len() {
                    diff.push_str(&format!("+{a}\n"));
                }
            }
        }

        diff
    }

    /// Get summary stats.
    pub fn stats(&self) -> FileHistoryStats {
        let files_changed: std::collections::HashSet<_> = self
            .changes
            .iter()
            .filter(|c| {
                matches!(
                    c.change_type,
                    ChangeType::Write | ChangeType::Edit | ChangeType::Create
                )
            })
            .map(|c| c.path.to_string_lossy().to_string())
            .collect();

        FileHistoryStats {
            total_changes: self.changes.len(),
            files_changed: files_changed.len(),
            files_read: self.access_counts.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistoryStats {
    pub total_changes: usize,
    pub files_changed: usize,
    pub files_read: usize,
}
