//! Project memory — FORGEFLEET.md hierarchical discovery and context injection.
//!
//! Discovers project memory files from:
//! 1. Current working directory (FORGEFLEET.md)
//! 2. Parent directories up to filesystem root
//! 3. Home directory (~/.forgefleet/FORGEFLEET.md)
//!
//! These files provide persistent project context that's injected into the
//! system prompt for every agent session.

use std::path::{Path, PathBuf};

use tokio::fs;
use tracing::debug;

/// The filename for project memory files.
const MEMORY_FILENAME: &str = "FORGEFLEET.md";

/// Discover all FORGEFLEET.md files from cwd up to root + home directory.
/// Returns them in order: most specific (cwd) first, most general (home) last.
pub async fn discover_memory_files(working_dir: &Path) -> Vec<MemoryFile> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Walk from cwd up to root
    let mut current = working_dir.to_path_buf();
    loop {
        let candidate = current.join(MEMORY_FILENAME);
        if let Ok(content) = fs::read_to_string(&candidate).await {
            let canonical = candidate.canonicalize().unwrap_or(candidate.clone());
            if seen.insert(canonical.clone()) {
                debug!(path = %candidate.display(), "found project memory file");
                files.push(MemoryFile {
                    path: candidate,
                    content,
                    scope: if current == working_dir {
                        MemoryScope::Project
                    } else {
                        MemoryScope::Parent
                    },
                });
            }
        }

        if !current.pop() {
            break;
        }
    }

    // Check home directory
    if let Some(home) = dirs::home_dir() {
        let home_file = home.join(".forgefleet").join(MEMORY_FILENAME);
        if let Ok(content) = fs::read_to_string(&home_file).await {
            let canonical = home_file.canonicalize().unwrap_or(home_file.clone());
            if seen.insert(canonical) {
                debug!(path = %home_file.display(), "found global memory file");
                files.push(MemoryFile {
                    path: home_file,
                    content,
                    scope: MemoryScope::Global,
                });
            }
        }
    }

    files
}

/// A discovered memory file.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub content: String,
    pub scope: MemoryScope,
}

/// Where the memory file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// In the project's working directory.
    Project,
    /// In a parent directory.
    Parent,
    /// In ~/.forgefleet/
    Global,
}

impl MemoryScope {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Parent => "parent",
            Self::Global => "global",
        }
    }
}

/// Detect the git root for a working directory.
/// Returns None if not in a git repo or git is not available.
pub async fn detect_git_root(cwd: &Path) -> Option<PathBuf> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// Build a context section from discovered memory files for injection into
/// the system prompt.
pub fn build_memory_context(files: &[MemoryFile]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut context = String::from("\n\n## Project Memory\n\n");
    for file in files {
        let label = file.scope.label();
        context.push_str(&format!(
            "### FORGEFLEET.md ({label}: {})\n\n{}\n\n",
            file.path.display(),
            file.content.trim()
        ));
    }
    context
}
