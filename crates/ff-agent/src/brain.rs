//! Three-brain memory architecture — Hive Mind, Fleet Brain, Project Memory.
//!
//! Loads context from three layers and builds a prioritized injection for the
//! system prompt. Each layer serves a different purpose:
//!
//! - **Project Memory** (`{git_root}/.forgefleet/`) — project-specific, travels with code
//! - **Fleet Brain** (`~/.forgefleet/brain/`) — personal preferences, cross-project patterns
//! - **Hive Mind** (`~/.forgefleet/hive/`) — shared fleet standards, synced across fleet

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info};

use crate::memory;
use crate::scoped_memory::MemoryEntry;

// ---------------------------------------------------------------------------
// Brain context — loaded from all three layers
// ---------------------------------------------------------------------------

/// The combined context from all three memory layers.
#[derive(Debug, Clone, Default)]
pub struct BrainContext {
    // Layer 1: Project Memory (most specific)
    pub project_forgefleet_md: Option<String>,
    pub project_context_md: Option<String>,
    pub project_entries: Vec<MemoryEntry>,

    // Layer 2: Fleet Brain (personal)
    pub brain_md: Option<String>,
    pub brain_entries: Vec<MemoryEntry>,

    // Layer 3: Hive Mind (shared fleet)
    pub hive_md: Option<String>,
    pub hive_entries: Vec<MemoryEntry>,

    // Metadata
    pub project_root: Option<PathBuf>,
    pub project_name: Option<String>,
}

/// Status for TUI display.
#[derive(Debug, Clone, Default)]
pub struct BrainLoadedStatus {
    pub project_entries: usize,
    pub brain_entries: usize,
    pub hive_entries: usize,
    pub project_name: Option<String>,
    pub hive_synced_at: Option<DateTime<Utc>>,
}

impl From<&BrainContext> for BrainLoadedStatus {
    fn from(ctx: &BrainContext) -> Self {
        Self {
            project_entries: ctx.project_entries.len()
                + ctx.project_forgefleet_md.as_ref().map(|_| 1).unwrap_or(0)
                + ctx.project_context_md.as_ref().map(|_| 1).unwrap_or(0),
            brain_entries: ctx.brain_entries.len() + ctx.brain_md.as_ref().map(|_| 1).unwrap_or(0),
            hive_entries: ctx.hive_entries.len() + ctx.hive_md.as_ref().map(|_| 1).unwrap_or(0),
            project_name: ctx.project_name.clone(),
            hive_synced_at: None, // set by caller from hive_sync
        }
    }
}

// ---------------------------------------------------------------------------
// Brain loader
// ---------------------------------------------------------------------------

/// Loads all three memory layers and builds the injection string.
pub struct BrainLoader;

impl BrainLoader {
    /// Load all three brains for a working directory.
    /// Runs project root detection + all three loads in parallel.
    pub async fn load_for_dir(cwd: &Path) -> BrainContext {
        let project_root = memory::detect_git_root(cwd).await;

        // Load all three layers in parallel
        let (brain_result, hive_result) = tokio::join!(load_fleet_brain(), load_hive_mind(),);

        let project_result = if let Some(root) = &project_root {
            // Auto-generate context.md if missing
            ensure_project_context(root).await;
            load_project_memory(root).await
        } else {
            ProjectMemoryResult::default()
        };

        let project_name = project_root
            .as_ref()
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().to_string());

        let ctx = BrainContext {
            project_forgefleet_md: project_result.forgefleet_md,
            project_context_md: project_result.context_md,
            project_entries: project_result.entries,
            brain_md: brain_result.0,
            brain_entries: brain_result.1,
            hive_md: hive_result.0,
            hive_entries: hive_result.1,
            project_root,
            project_name,
        };

        let status = BrainLoadedStatus::from(&ctx);
        info!(
            project = status.project_entries,
            brain = status.brain_entries,
            hive = status.hive_entries,
            "brain context loaded"
        );

        ctx
    }

    /// Build the injection string for the system prompt.
    /// Respects a token budget (estimated at ~4 chars/token).
    pub fn build_injection(ctx: &BrainContext, budget_tokens: usize) -> String {
        let budget_chars = budget_tokens * 4;
        let mut result = String::new();
        let mut remaining = budget_chars;

        // Header
        let header = "\n\n## Memory Context\n\n";
        result.push_str(header);
        remaining = remaining.saturating_sub(header.len());

        // Layer 1: Project Memory (highest priority — gets 50% of budget)
        let project_budget = remaining / 2;
        let project_section = build_project_section(ctx, project_budget);
        if !project_section.is_empty() {
            result.push_str(&project_section);
            remaining = remaining.saturating_sub(project_section.len());
        }

        // Layer 2: Fleet Brain (30% of remaining)
        let brain_budget = remaining * 3 / 10;
        let brain_section = build_brain_section(ctx, brain_budget);
        if !brain_section.is_empty() {
            result.push_str(&brain_section);
            remaining = remaining.saturating_sub(brain_section.len());
        }

        // Layer 3: Hive Mind (whatever's left)
        let hive_section = build_hive_section(ctx, remaining);
        if !hive_section.is_empty() {
            result.push_str(&hive_section);
        }

        // Only return if we actually have content
        if result.trim().len() <= "## Memory Context".len() + 5 {
            return String::new();
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Layer loaders
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ProjectMemoryResult {
    forgefleet_md: Option<String>,
    context_md: Option<String>,
    entries: Vec<MemoryEntry>,
}

async fn load_project_memory(project_root: &Path) -> ProjectMemoryResult {
    let ff_dir = project_root.join(".forgefleet");

    // Auto-create .forgefleet/ if it doesn't exist
    if !ff_dir.exists() {
        let _ = fs::create_dir_all(&ff_dir).await;
        // Create .gitignore for sessions
        let gitignore = ff_dir.join(".gitignore");
        if !gitignore.exists() {
            let _ = fs::write(&gitignore, "sessions/\n*.tmp\n").await;
        }
    }

    let forgefleet_md = read_optional(&ff_dir.join("FORGEFLEET.md")).await;
    let context_md = read_optional(&ff_dir.join("context.md")).await;
    let entries = load_entries_json(&ff_dir.join("memory").join("entries.json")).await;

    // Also check for FORGEFLEET.md at project root (not just .forgefleet/)
    let root_md = if forgefleet_md.is_none() {
        read_optional(&project_root.join("FORGEFLEET.md")).await
    } else {
        None
    };

    ProjectMemoryResult {
        forgefleet_md: forgefleet_md.or(root_md),
        context_md,
        entries,
    }
}

async fn load_fleet_brain() -> (Option<String>, Vec<MemoryEntry>) {
    let brain_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("brain");

    if !brain_dir.exists() {
        let _ = fs::create_dir_all(&brain_dir).await;
        // Create starter BRAIN.md
        let brain_md = brain_dir.join("BRAIN.md");
        if !brain_md.exists() {
            let _ = fs::write(
                &brain_md,
                "# Fleet Brain\n\nPersonal preferences and cross-project patterns.\n\
                 ForgeFleet learns from your sessions and stores patterns here.\n",
            )
            .await;
        }
    }

    let md = read_optional(&brain_dir.join("BRAIN.md")).await;
    let entries = load_entries_json(&brain_dir.join("learnings.json")).await;
    (md, entries)
}

async fn load_hive_mind() -> (Option<String>, Vec<MemoryEntry>) {
    let hive_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("hive");

    if !hive_dir.exists() {
        // Don't auto-create hive — it should be initialized by hive_sync
        return (None, Vec::new());
    }

    let md = read_optional(&hive_dir.join("HIVE.md")).await;
    let entries = load_entries_json(&hive_dir.join("learnings.json")).await;
    (md, entries)
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn build_project_section(ctx: &BrainContext, budget: usize) -> String {
    let mut section = String::new();
    let mut remaining = budget;

    let name = ctx.project_name.as_deref().unwrap_or("unknown");
    let header = format!("### Project: {name}\n");
    section.push_str(&header);
    remaining = remaining.saturating_sub(header.len());

    // FORGEFLEET.md instructions
    if let Some(md) = &ctx.project_forgefleet_md {
        let content = truncate(md, remaining / 2);
        if !content.is_empty() {
            let block = format!("**Instructions:**\n{content}\n\n");
            section.push_str(&block);
            remaining = remaining.saturating_sub(block.len());
        }
    }

    // context.md architecture
    if let Some(md) = &ctx.project_context_md {
        let content = truncate(md, remaining / 3);
        if !content.is_empty() {
            let block = format!("**Architecture:**\n{content}\n\n");
            section.push_str(&block);
            remaining = remaining.saturating_sub(block.len());
        }
    }

    // Structured entries (sorted by relevance)
    if !ctx.project_entries.is_empty() {
        section.push_str("**Known facts:**\n");
        let mut entries: Vec<_> = ctx.project_entries.iter().collect();
        entries.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for entry in entries.iter().take(10) {
            let line = format!("[{:?}] {}\n", entry.category, entry.content);
            if line.len() > remaining {
                break;
            }
            section.push_str(&line);
            remaining = remaining.saturating_sub(line.len());
        }
    }

    section.push('\n');
    section
}

fn build_brain_section(ctx: &BrainContext, budget: usize) -> String {
    if ctx.brain_md.is_none() && ctx.brain_entries.is_empty() {
        return String::new();
    }

    let mut section = String::from("### Personal Preferences (Fleet Brain)\n");
    let mut remaining = budget.saturating_sub(section.len());

    if let Some(md) = &ctx.brain_md {
        // Only inject the first paragraph of BRAIN.md (preamble)
        let preamble = md.split("\n\n").next().unwrap_or("");
        if !preamble.trim().is_empty() && !preamble.starts_with('#') {
            let content = truncate(preamble, remaining / 3);
            section.push_str(&format!("{content}\n"));
            remaining = remaining.saturating_sub(content.len() + 1);
        }
    }

    let mut entries: Vec<_> = ctx.brain_entries.iter().collect();
    entries.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for entry in entries.iter().take(10) {
        let line = format!("[{:?}] {}\n", entry.category, entry.content);
        if line.len() > remaining {
            break;
        }
        section.push_str(&line);
        remaining = remaining.saturating_sub(line.len());
    }

    section.push('\n');
    section
}

fn build_hive_section(ctx: &BrainContext, budget: usize) -> String {
    if ctx.hive_md.is_none() && ctx.hive_entries.is_empty() {
        return String::new();
    }

    let mut section = String::from("### Fleet Standards (Hive Mind)\n");
    let mut remaining = budget.saturating_sub(section.len());

    if let Some(md) = &ctx.hive_md {
        let preamble = md.split("\n\n").next().unwrap_or("");
        if !preamble.trim().is_empty() && !preamble.starts_with('#') {
            let content = truncate(preamble, remaining / 3);
            section.push_str(&format!("{content}\n"));
            remaining = remaining.saturating_sub(content.len() + 1);
        }
    }

    let mut entries: Vec<_> = ctx.hive_entries.iter().collect();
    entries.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for entry in entries.iter().take(10) {
        let line = format!("[{:?}] {}\n", entry.category, entry.content);
        if line.len() > remaining {
            break;
        }
        section.push_str(&line);
        remaining = remaining.saturating_sub(line.len());
    }

    section.push('\n');
    section
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn read_optional(path: &Path) -> Option<String> {
    match fs::read_to_string(path).await {
        Ok(content) if !content.trim().is_empty() => {
            debug!(path = %path.display(), "loaded brain file");
            Some(content)
        }
        _ => None,
    }
}

async fn load_entries_json(path: &Path) -> Vec<MemoryEntry> {
    match fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.trim().to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", s[..end].trim())
    }
}

// ---------------------------------------------------------------------------
// Context.md auto-generation
// ---------------------------------------------------------------------------

/// Auto-generate a context.md for a project by scanning its file tree.
/// Only runs if context.md doesn't already exist.
pub async fn ensure_project_context(project_root: &Path) {
    let context_path = project_root.join(".forgefleet").join("context.md");
    if context_path.exists() {
        return;
    }

    // Scan the project for key files
    let mut summary = String::from("# Project Architecture\n\n");
    summary.push_str(&format!(
        "Project: {}\n",
        project_root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    summary.push_str(&format!("Root: {}\n\n", project_root.display()));

    // Detect tech stack
    let mut stack = Vec::new();
    if project_root.join("Cargo.toml").exists() {
        stack.push("Rust (Cargo)");
    }
    if project_root.join("package.json").exists() {
        stack.push("JavaScript/TypeScript (npm)");
    }
    if project_root.join("pyproject.toml").exists() || project_root.join("setup.py").exists() {
        stack.push("Python");
    }
    if project_root.join("go.mod").exists() {
        stack.push("Go");
    }
    if project_root.join("Dockerfile").exists() {
        stack.push("Docker");
    }
    if project_root.join("docker-compose.yml").exists()
        || project_root.join("docker-compose.yaml").exists()
    {
        stack.push("Docker Compose");
    }
    if project_root.join(".github").exists() {
        stack.push("GitHub Actions");
    }
    if project_root.join("Makefile").exists() {
        stack.push("Make");
    }

    if !stack.is_empty() {
        summary.push_str("## Tech Stack\n");
        for s in &stack {
            summary.push_str(&format!("- {s}\n"));
        }
        summary.push('\n');
    }

    // List top-level directories
    summary.push_str("## Structure\n");
    if let Ok(mut entries) = fs::read_dir(project_root).await {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if name == "target" || name == "node_modules" || name == "dist" || name == "__pycache__"
            {
                continue;
            }
            if entry.path().is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();
        for d in &dirs {
            summary.push_str(&format!("- {d}/\n"));
        }
        for f in files.iter().take(10) {
            summary.push_str(&format!("- {f}\n"));
        }
    }

    summary.push_str("\n*Auto-generated by ForgeFleet. Edit to add project-specific context.*\n");

    // Write it
    if let Some(parent) = context_path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let _ = fs::write(&context_path, &summary).await;
    info!(path = %context_path.display(), "auto-generated project context.md");
}

// ---------------------------------------------------------------------------
// Memory search — search across all three brains
// ---------------------------------------------------------------------------

/// Search across all three brain layers for entries matching a query.
pub async fn search_all(query: &str, cwd: &Path) -> Vec<SearchResult> {
    let ctx = BrainLoader::load_for_dir(cwd).await;
    let lower = query.to_ascii_lowercase();
    let mut results = Vec::new();

    // Search project entries
    for entry in &ctx.project_entries {
        if entry.content.to_ascii_lowercase().contains(&lower) {
            results.push(SearchResult {
                layer: "Project".into(),
                category: format!("{:?}", entry.category),
                content: entry.content.clone(),
                relevance: entry.relevance,
            });
        }
    }

    // Search project markdown files
    if let Some(md) = &ctx.project_forgefleet_md {
        if md.to_ascii_lowercase().contains(&lower) {
            results.push(SearchResult {
                layer: "Project".into(),
                category: "FORGEFLEET.md".into(),
                content: truncate(md, 200),
                relevance: 1.0,
            });
        }
    }

    // Search brain entries
    for entry in &ctx.brain_entries {
        if entry.content.to_ascii_lowercase().contains(&lower) {
            results.push(SearchResult {
                layer: "Brain".into(),
                category: format!("{:?}", entry.category),
                content: entry.content.clone(),
                relevance: entry.relevance,
            });
        }
    }

    // Search hive entries
    for entry in &ctx.hive_entries {
        if entry.content.to_ascii_lowercase().contains(&lower) {
            results.push(SearchResult {
                layer: "Hive".into(),
                category: format!("{:?}", entry.category),
                content: entry.content.clone(),
                relevance: entry.relevance,
            });
        }
    }

    // Sort by relevance
    results.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub layer: String,
    pub category: String,
    pub content: String,
    pub relevance: f64,
}
