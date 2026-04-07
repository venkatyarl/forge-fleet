//! Skill registry — discover, index, search, and manage skills.
//!
//! The registry is the central catalog of all known skills.  It uses `DashMap`
//! for lock-free concurrent access so multiple tasks can register / query
//! skills simultaneously.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use dashmap::DashMap;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::{Result, SkillError};
use crate::loader;
use crate::types::{SkillMetadata, SkillOrigin};

// ─── Registry ────────────────────────────────────────────────────────────────

/// Thread-safe skill registry backed by `DashMap`.
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    /// Skills indexed by their `id`.
    skills: Arc<DashMap<String, SkillMetadata>>,
    /// Scan directories (roots) where skills are discovered.
    scan_dirs: Arc<Vec<PathBuf>>,
}

impl SkillRegistry {
    /// Create an empty registry with the given scan directories.
    pub fn new(scan_dirs: Vec<PathBuf>) -> Self {
        Self {
            skills: Arc::new(DashMap::new()),
            scan_dirs: Arc::new(scan_dirs),
        }
    }

    /// Create an empty registry with no scan dirs.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    // ── Registration ─────────────────────────────────────────────────

    /// Register a skill.  Returns error if a skill with the same id exists.
    pub fn register(&self, mut skill: SkillMetadata) -> Result<()> {
        skill.rebuild_keywords();
        if self.skills.contains_key(&skill.id) {
            return Err(SkillError::AlreadyRegistered {
                name: skill.id.clone(),
            });
        }
        info!(skill = %skill.id, origin = %skill.origin, tools = skill.tool_count(), "registered skill");
        self.skills.insert(skill.id.clone(), skill);
        Ok(())
    }

    /// Register or update a skill (upsert).
    pub fn upsert(&self, mut skill: SkillMetadata) {
        skill.rebuild_keywords();
        info!(skill = %skill.id, origin = %skill.origin, tools = skill.tool_count(), "upserted skill");
        self.skills.insert(skill.id.clone(), skill);
    }

    /// Remove a skill by id.
    pub fn remove(&self, id: &str) -> Option<SkillMetadata> {
        self.skills.remove(id).map(|(_, v)| v)
    }

    // ── Lookup ───────────────────────────────────────────────────────

    /// Get a skill by id.
    pub fn get(&self, id: &str) -> Option<SkillMetadata> {
        self.skills.get(id).map(|r| r.value().clone())
    }

    /// Check if a skill with the given id exists.
    pub fn contains(&self, id: &str) -> bool {
        self.skills.contains_key(id)
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Return all registered skills.
    pub fn list_all(&self) -> Vec<SkillMetadata> {
        self.skills.iter().map(|r| r.value().clone()).collect()
    }

    /// List skills from a specific origin.
    pub fn list_by_origin(&self, origin: SkillOrigin) -> Vec<SkillMetadata> {
        self.skills
            .iter()
            .filter(|r| r.value().origin == origin)
            .map(|r| r.value().clone())
            .collect()
    }

    // ── Search ───────────────────────────────────────────────────────

    /// Simple keyword search across skill names, descriptions, and tags.
    ///
    /// Returns skills sorted by relevance (number of matching keywords).
    pub fn search(&self, query: &str) -> Vec<SkillMetadata> {
        let query_tokens: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| s.len() >= 2)
            .collect();

        if query_tokens.is_empty() {
            return self.list_all();
        }

        let mut scored: Vec<(SkillMetadata, usize)> = self
            .skills
            .iter()
            .filter_map(|entry| {
                let skill = entry.value();
                let score = compute_search_score(skill, &query_tokens);
                if score > 0 {
                    Some((skill.clone(), score))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.into_iter().map(|(s, _)| s).collect()
    }

    /// Find skills that provide a specific tool name.
    pub fn find_by_tool(&self, tool_name: &str) -> Vec<SkillMetadata> {
        self.skills
            .iter()
            .filter(|r| r.value().tools.iter().any(|t| t.name == tool_name))
            .map(|r| r.value().clone())
            .collect()
    }

    // ── Discovery ────────────────────────────────────────────────────

    /// Scan all configured directories for skills and register them.
    ///
    /// Each directory is expected to contain subdirectories, each being a skill
    /// (e.g. with a `SKILL.md`, `mcp.json`, or tool scripts).
    pub async fn discover_all(&self) -> Result<usize> {
        let mut total = 0usize;
        for dir in self.scan_dirs.iter() {
            match self.discover_directory(dir).await {
                Ok(n) => total += n,
                Err(e) => warn!(dir = %dir.display(), error = %e, "failed to scan skill directory"),
            }
        }
        info!(
            total,
            dirs = self.scan_dirs.len(),
            "skill discovery complete"
        );
        Ok(total)
    }

    /// Scan a single directory for skills.
    pub async fn discover_directory(&self, dir: &Path) -> Result<usize> {
        if !dir.is_dir() {
            return Err(SkillError::DirectoryNotFound {
                path: dir.to_path_buf(),
            });
        }

        let mut count = 0usize;
        let mut entries = tokio::fs::read_dir(dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            match load_skill_from_dir(&path).await {
                Ok(skill) => {
                    self.upsert(skill);
                    count += 1;
                }
                Err(e) => {
                    debug!(dir = %path.display(), error = %e, "skipping directory (not a valid skill)");
                }
            }
        }

        info!(dir = %dir.display(), skills = count, "scanned skill directory");
        Ok(count)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Compute search relevance score for a skill against query tokens.
fn compute_search_score(skill: &SkillMetadata, tokens: &[String]) -> usize {
    let mut score = 0usize;
    for token in tokens {
        // Exact name match is highest weight.
        if skill.id.to_lowercase().contains(token) {
            score += 10;
        }
        if skill.name.to_lowercase().contains(token) {
            score += 8;
        }
        // Tag match.
        for tag in &skill.tags {
            if tag.to_lowercase().contains(token) {
                score += 5;
            }
        }
        // Keyword index match.
        for kw in &skill.search_keywords {
            if kw.contains(token) {
                score += 2;
            }
        }
        // Tool name match.
        for tool in &skill.tools {
            if tool.name.to_lowercase().contains(token) {
                score += 4;
            }
        }
    }
    score
}

/// Try to load a skill from a directory by probing known file formats.
async fn load_skill_from_dir(dir: &Path) -> Result<SkillMetadata> {
    // Priority: SKILL.md → mcp.json → tools.json → fallback
    let skill_md = dir.join("SKILL.md");
    if skill_md.exists() {
        return loader::load_openclaw_skill(&skill_md).await;
    }

    let mcp_json = dir.join("mcp.json");
    if mcp_json.exists() {
        return loader::load_mcp_tools(&mcp_json).await;
    }

    let tools_json = dir.join("tools.json");
    if tools_json.exists() {
        return loader::load_mcp_tools(&tools_json).await;
    }

    // Fallback: create a minimal skill entry from the directory name.
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(SkillMetadata {
        id: name.clone(),
        name: name.clone(),
        description: format!("Auto-discovered skill from {}", dir.display()),
        origin: SkillOrigin::Filesystem,
        location: Some(dir.to_path_buf()),
        version: None,
        author: None,
        tags: Vec::new(),
        tools: Vec::new(),
        permissions: Vec::new(),
        registered_at: Utc::now(),
        uuid: Uuid::new_v4(),
        search_keywords: Vec::new(),
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolDefinition, ToolInvocation};

    fn sample_skill(id: &str, tags: &[&str]) -> SkillMetadata {
        SkillMetadata {
            id: id.to_string(),
            name: id.to_string(),
            description: format!("Test skill {id}"),
            origin: SkillOrigin::OpenClaw,
            location: None,
            version: None,
            author: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            tools: vec![ToolDefinition {
                name: format!("{id}_tool"),
                description: "A test tool".into(),
                parameters: Vec::new(),
                invocation: ToolInvocation::Builtin {
                    handler: "test".into(),
                },
                permissions: Vec::new(),
                timeout_secs: 30,
            }],
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        }
    }

    #[test]
    fn test_register_and_get() {
        let reg = SkillRegistry::empty();
        let skill = sample_skill("weather", &["weather", "forecast"]);
        reg.register(skill).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("weather"));
        let got = reg.get("weather").unwrap();
        assert_eq!(got.id, "weather");
    }

    #[test]
    fn test_duplicate_registration() {
        let reg = SkillRegistry::empty();
        let s1 = sample_skill("dup", &[]);
        let s2 = sample_skill("dup", &[]);
        reg.register(s1).unwrap();
        assert!(reg.register(s2).is_err());
    }

    #[test]
    fn test_upsert() {
        let reg = SkillRegistry::empty();
        let s1 = sample_skill("up", &["v1"]);
        reg.upsert(s1);
        assert_eq!(reg.len(), 1);
        let s2 = sample_skill("up", &["v2"]);
        reg.upsert(s2);
        assert_eq!(reg.len(), 1);
        let got = reg.get("up").unwrap();
        assert!(got.tags.contains(&"v2".to_string()));
    }

    #[test]
    fn test_search() {
        let reg = SkillRegistry::empty();
        reg.upsert(sample_skill("weather", &["forecast", "temperature"]));
        reg.upsert(sample_skill("calendar", &["schedule", "events"]));
        reg.upsert(sample_skill("email", &["inbox", "send"]));

        let results = reg.search("weather forecast");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "weather");
    }

    #[test]
    fn test_find_by_tool() {
        let reg = SkillRegistry::empty();
        reg.upsert(sample_skill("weather", &[]));
        let found = reg.find_by_tool("weather_tool");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "weather");
    }

    #[test]
    fn test_remove() {
        let reg = SkillRegistry::empty();
        reg.upsert(sample_skill("rm", &[]));
        assert!(reg.contains("rm"));
        reg.remove("rm");
        assert!(!reg.contains("rm"));
    }
}
