//! Scoped memory system — auto-managed, date-organized, project-aware memory.
//!
//! ForgeFleet manages its own memory structure automatically:
//!
//! ```text
//! ~/.forgefleet/memory/
//! ├── global/                          ← ForgeFleet-wide memory
//! │   └── FORGEFLEET.md
//! ├── projects/
//! │   ├── {project-name}/
//! │   │   ├── project.md               ← project-level memory
//! │   │   └── chats/
//! │   │       └── {YYYY}/{YYYY-MM}/{MMDDYYYY}/
//! │   │           └── {NN}-{MMDDYYYY}-{name}.json
//! │   └── ...
//! ├── temp/
//! │   └── {YYYY}/{YYYY-MM}/{MMDDYYYY}/
//! │       └── {NN}-{MMDDYYYY}-{name}.json
//! └── daily/
//!     └── {YYYY}/{YYYY-MM}/{MMDDYYYY}.md
//! ```
//!
//! All folders are created automatically on first use. No manual setup needed.

use std::path::PathBuf;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Memory scope types
// ---------------------------------------------------------------------------

/// Where memory is scoped to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryScope {
    /// Global ForgeFleet memory.
    Global,
    /// Scoped to a specific project.
    Project { project_id: String, project_name: String },
    /// Merged context from multiple projects.
    MultiProject { project_ids: Vec<String>, project_names: Vec<String> },
    /// Scoped to a specific filesystem directory.
    Folder { path: PathBuf },
    /// Temporary — cleaned up after scrub.
    Temp,
}

impl MemoryScope {
    pub fn display_name(&self) -> String {
        match self {
            Self::Global => "ForgeFleet (Global)".into(),
            Self::Project { project_name, .. } => format!("Project: {project_name}"),
            Self::MultiProject { project_names, .. } => format!("Multi: {}", project_names.join(", ")),
            Self::Folder { path } => format!("Folder: {}", path.display()),
            Self::Temp => "Temp".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Date-based path generation
// ---------------------------------------------------------------------------

/// Generate the date-based folder path: {YYYY}/{YYYY-MM}/{MMDDYYYY}/
fn date_path(date: &DateTime<Utc>) -> PathBuf {
    let year = format!("{}", date.year());
    let year_month = format!("{}-{:02}", date.year(), date.month());
    let day_folder = format!("{:02}{:02}{}", date.month(), date.day(), date.year());
    PathBuf::from(year).join(year_month).join(day_folder)
}

/// Generate a chat filename: {NN}-{MMDDYYYY}-{name}.json
fn chat_filename(sequence: u32, date: &DateTime<Utc>, name: &str) -> String {
    let date_str = format!("{:02}{:02}{}", date.month(), date.day(), date.year());
    let safe_name = sanitize_name(name);
    format!("{:02}-{date_str}-{safe_name}.json", sequence)
}

/// Sanitize a name for use in filenames.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
        .chars()
        .take(40)
        .collect()
}

// ---------------------------------------------------------------------------
// Memory entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub category: MemoryCategory,
    pub content: String,
    pub relevance: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub source_session: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    Fact,
    Decision,
    Preference,
    Constraint,
    Learning,
    Context,
}

// ---------------------------------------------------------------------------
// Auto-managed memory store
// ---------------------------------------------------------------------------

/// ForgeFleet's auto-managed memory store. Creates all directory structures automatically.
pub struct ScopedMemoryStore {
    scope: MemoryScope,
    entries: Vec<MemoryEntry>,
    base_path: PathBuf,
    dirty: bool,
}

impl ScopedMemoryStore {
    /// Open a memory store for a scope. Creates directories automatically.
    pub async fn open(scope: MemoryScope) -> Self {
        let base_path = scope_base_path(&scope);

        // Auto-create the directory structure
        if let Err(e) = fs::create_dir_all(&base_path).await {
            warn!(path = %base_path.display(), error = %e, "failed to create memory directory");
        }

        // Auto-create project.md if it doesn't exist
        if let MemoryScope::Project { project_name, .. } = &scope {
            let project_md = base_path.join("project.md");
            if !project_md.exists() {
                let content = format!("# {project_name}\n\nProject memory for {project_name}.\nAdd project-specific context, conventions, and decisions here.\n");
                let _ = fs::write(&project_md, content).await;
            }
        }

        // Auto-create global FORGEFLEET.md
        if scope == MemoryScope::Global {
            let global_md = base_path.join("FORGEFLEET.md");
            if !global_md.exists() {
                let content = "# ForgeFleet Memory\n\nGlobal memory for ForgeFleet.\nAdd fleet-wide context, preferences, and learnings here.\n";
                let _ = fs::write(&global_md, content).await;
            }
        }

        let entries = load_entries(&base_path).await;
        info!(scope = %scope.display_name(), entries = entries.len(), path = %base_path.display(), "opened memory store");

        Self { scope, entries, base_path, dirty: false }
    }

    /// Add a memory entry.
    pub fn add(&mut self, category: MemoryCategory, content: String, tags: Vec<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        self.entries.push(MemoryEntry {
            id: id.clone(), category, content, relevance: 0.5,
            created_at: now, updated_at: now, source_session: None, tags,
        });
        self.dirty = true;
        id
    }

    /// Get all memories sorted by relevance.
    pub fn all(&self) -> Vec<&MemoryEntry> {
        let mut sorted: Vec<_> = self.entries.iter().collect();
        sorted.sort_by(|a, b| b.relevance.partial_cmp(&a.relevance).unwrap_or(std::cmp::Ordering::Equal));
        sorted
    }

    /// Search memories by keyword.
    pub fn search(&self, query: &str) -> Vec<&MemoryEntry> {
        let lower = query.to_ascii_lowercase();
        self.entries.iter()
            .filter(|e| e.content.to_ascii_lowercase().contains(&lower)
                || e.tags.iter().any(|t| t.to_ascii_lowercase().contains(&lower)))
            .collect()
    }

    /// Build context string for system prompt injection.
    pub fn build_context(&self, max_chars: usize) -> String {
        let mut context = String::new();
        let mut used = 0;
        for entry in self.all() {
            let line = format!("[{}] {}\n", category_label(entry.category), entry.content);
            if used + line.len() > max_chars { break; }
            context.push_str(&line);
            used += line.len();
        }
        context
    }

    /// Save entries to the date-organized path.
    pub async fn save(&self) -> anyhow::Result<()> {
        let entries_path = self.base_path.join("entries.json");
        let json = serde_json::to_string_pretty(&self.entries)?;
        fs::write(&entries_path, json).await?;
        debug!(scope = %self.scope.display_name(), path = %entries_path.display(), "saved memory");
        Ok(())
    }

    /// Save a chat session to the date-organized structure.
    pub async fn save_chat(
        &self,
        chat_name: &str,
        chat_data: &serde_json::Value,
    ) -> anyhow::Result<PathBuf> {
        let now = Utc::now();
        let chats_dir = self.base_path.join("chats").join(date_path(&now));
        fs::create_dir_all(&chats_dir).await?;

        // Count existing chats today to get sequence number
        let sequence = count_files_in_dir(&chats_dir).await + 1;
        let filename = chat_filename(sequence as u32, &now, chat_name);
        let path = chats_dir.join(&filename);

        let json = serde_json::to_string_pretty(chat_data)?;
        fs::write(&path, json).await?;

        info!(path = %path.display(), "saved chat to date-organized structure");
        Ok(path)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() < before { self.dirty = true; true } else { false }
    }

    pub fn boost_relevance(&mut self, id: &str, boost: f64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.relevance = (entry.relevance + boost).min(1.0);
            entry.updated_at = Utc::now();
            self.dirty = true;
        }
    }

    pub fn scope(&self) -> &MemoryScope { &self.scope }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

fn category_label(cat: MemoryCategory) -> &'static str {
    match cat {
        MemoryCategory::Fact => "FACT",
        MemoryCategory::Decision => "DECISION",
        MemoryCategory::Preference => "PREF",
        MemoryCategory::Constraint => "CONSTRAINT",
        MemoryCategory::Learning => "LEARNING",
        MemoryCategory::Context => "CONTEXT",
    }
}

// ---------------------------------------------------------------------------
// Daily summary
// ---------------------------------------------------------------------------

/// Generate a daily summary file at daily/{YYYY}/{YYYY-MM}/{MMDDYYYY}.md
pub async fn generate_daily_summary(date: &DateTime<Utc>, summary_content: &str) -> anyhow::Result<PathBuf> {
    let daily_dir = memory_base_dir().join("daily").join(date_path(date));
    // The date_path gives us year/year-month/MMDDYYYY — we want the file at the day level
    let parent = daily_dir.parent().unwrap_or(&daily_dir);
    fs::create_dir_all(parent).await?;

    let filename = format!("{:02}{:02}{}.md", date.month(), date.day(), date.year());
    let path = parent.join(filename);

    let content = format!(
        "# Daily Summary — {}\n\n{summary_content}\n",
        date.format("%B %d, %Y")
    );

    fs::write(&path, content).await?;
    info!(path = %path.display(), "generated daily summary");
    Ok(path)
}

// ---------------------------------------------------------------------------
// Temp scrub
// ---------------------------------------------------------------------------

/// Scrub temp memories — extract valuable data, then clean up.
pub async fn scrub_temp_sessions() -> anyhow::Result<u32> {
    let temp_dir = memory_base_dir().join("temp");
    if !temp_dir.exists() { return Ok(0); }

    let mut scrubbed = 0u32;

    // Walk year/month/day structure
    let mut year_entries = fs::read_dir(&temp_dir).await?;
    while let Some(year_entry) = year_entries.next_entry().await? {
        if !year_entry.path().is_dir() { continue; }
        let mut month_entries = fs::read_dir(year_entry.path()).await?;
        while let Some(month_entry) = month_entries.next_entry().await? {
            if !month_entry.path().is_dir() { continue; }
            let mut day_entries = fs::read_dir(month_entry.path()).await?;
            while let Some(day_entry) = day_entries.next_entry().await? {
                let path = day_entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    // Read temp chat, extract learnings to global memory
                    if let Ok(content) = fs::read_to_string(&path).await {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
                            // Extract any high-relevance entries
                            if let Some(entries) = data.get("entries").and_then(|v| v.as_array()) {
                                let mut global = ScopedMemoryStore::open(MemoryScope::Global).await;
                                for entry in entries {
                                    let relevance = entry.get("relevance").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    if relevance > 0.3 {
                                        if let Some(content) = entry.get("content").and_then(|v| v.as_str()) {
                                            global.add(MemoryCategory::Learning, format!("[scrubbed] {content}"), vec!["scrubbed".into()]);
                                        }
                                    }
                                }
                                let _ = global.save().await;
                            }
                        }
                    }
                    // Remove the temp file
                    let _ = fs::remove_file(&path).await;
                    scrubbed += 1;
                }
            }
        }
    }

    // Clean up empty directories
    let _ = cleanup_empty_dirs(&temp_dir).await;

    info!(scrubbed, "temp memory scrub complete");
    Ok(scrubbed)
}

// ---------------------------------------------------------------------------
// Multi-scope context
// ---------------------------------------------------------------------------

/// Build merged context from multiple memory scopes.
pub async fn build_multi_scope_context(scopes: &[MemoryScope], max_chars: usize) -> String {
    let mut context = String::new();
    let per_scope = max_chars / scopes.len().max(1);

    for scope in scopes {
        let store = ScopedMemoryStore::open(scope.clone()).await;
        if !store.is_empty() {
            context.push_str(&format!("\n### {}\n", scope.display_name()));
            context.push_str(&store.build_context(per_scope));
        }
    }
    context
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn memory_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("memory")
}

fn scope_base_path(scope: &MemoryScope) -> PathBuf {
    let base = memory_base_dir();
    match scope {
        MemoryScope::Global => base.join("global"),
        MemoryScope::Project { project_name, .. } => base.join("projects").join(sanitize_name(project_name)),
        MemoryScope::MultiProject { project_names, .. } => {
            let mut names = project_names.clone();
            names.sort();
            let hash = simple_hash(&names.join(","));
            base.join("multi").join(hash)
        }
        MemoryScope::Folder { path } => {
            let hash = simple_hash(&path.to_string_lossy());
            base.join("folders").join(hash)
        }
        MemoryScope::Temp => base.join("temp"),
    }
}

async fn load_entries(base_path: &std::path::Path) -> Vec<MemoryEntry> {
    let path = base_path.join("entries.json");
    match fs::read_to_string(&path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn count_files_in_dir(dir: &std::path::Path) -> usize {
    let mut count = 0;
    if let Ok(mut entries) = fs::read_dir(dir).await {
        while let Ok(Some(_)) = entries.next_entry().await {
            count += 1;
        }
    }
    count
}

async fn cleanup_empty_dirs(dir: &std::path::Path) -> anyhow::Result<()> {
    if let Ok(mut entries) = fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                Box::pin(cleanup_empty_dirs(&path)).await?;
                // Try to remove — will fail if not empty (which is fine)
                let _ = fs::remove_dir(&path).await;
            }
        }
    }
    Ok(())
}

fn simple_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_path_format() {
        use chrono::TimeZone;
        let date = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();
        let path = date_path(&date);
        assert_eq!(path, PathBuf::from("2026/2026-04/04062026"));
    }

    #[test]
    fn chat_filename_format() {
        use chrono::TimeZone;
        let date = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();
        let name = chat_filename(1, &date, "Fix Auth Bug");
        assert_eq!(name, "01-04062026-fix-auth-bug.json");
    }

    #[test]
    fn chat_filename_sequence() {
        use chrono::TimeZone;
        let date = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();
        assert_eq!(chat_filename(3, &date, "Review PR"), "03-04062026-review-pr.json");
    }

    #[test]
    fn scope_paths_are_distinct() {
        let global = scope_base_path(&MemoryScope::Global);
        let project = scope_base_path(&MemoryScope::Project { project_id: "1".into(), project_name: "Test".into() });
        let temp = scope_base_path(&MemoryScope::Temp);
        assert_ne!(global, project);
        assert_ne!(project, temp);
    }

    #[test]
    fn sanitize_name_works() {
        assert_eq!(sanitize_name("Fix Auth Bug"), "fix-auth-bug");
        assert_eq!(sanitize_name("hello world!@#"), "hello-world");
        assert_eq!(sanitize_name("  spaces  "), "spaces");
    }
}
