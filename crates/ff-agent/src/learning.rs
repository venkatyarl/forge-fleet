//! Auto-learning pipeline — extract knowledge from agent sessions and route
//! to the right brain layer automatically.
//!
//! After each session:
//! 1. Scan conversation for decisions, preferences, facts, patterns
//! 2. Route to Project Memory, Fleet Brain, or Hive Mind based on content
//! 3. Deduplicate against existing entries (boost relevance instead of adding)
//! 4. High-confidence fleet-wide learnings auto-promote to Hive Mind

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::brain::BrainContext;
use crate::scoped_memory::{MemoryCategory, MemoryEntry};
use ff_api::tool_calling::ToolChatMessage;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Where a learning should be stored.
#[derive(Debug, Clone)]
pub enum LearningSink {
    ProjectMemory { project_root: PathBuf },
    FleetBrain,
    HiveMind,
}

/// A candidate learning extracted from a conversation.
#[derive(Debug, Clone)]
struct LearningCandidate {
    content: String,
    category: MemoryCategory,
    confidence: f64,
    is_project_specific: bool,
}

/// Report of what was learned from a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningReport {
    pub project_count: usize,
    pub brain_count: usize,
    pub hive_count: usize,
    pub total_candidates: usize,
}

// ---------------------------------------------------------------------------
// Extraction patterns — keywords that signal learnable content
// ---------------------------------------------------------------------------

const DECISION_SIGNALS: &[&str] = &[
    "decided to", "we'll use", "going with", "chose", "switched to",
    "prefer", "instead of", "better to", "should always", "never use",
    "from now on", "let's go with", "the approach is",
];

const PREFERENCE_SIGNALS: &[&str] = &[
    "i prefer", "i like", "don't like", "always use", "never",
    "i want", "please always", "please don't", "stop doing",
    "keep doing", "that's perfect", "exactly right",
];

const FACT_SIGNALS: &[&str] = &[
    "the architecture", "the database", "uses postgresql", "uses sqlite",
    "runs on port", "the api", "the endpoint", "built with",
    "depends on", "configured at", "stored in", "deployed to",
];

const TOOL_PATTERN_SIGNALS: &[&str] = &[
    "use edit instead of", "use bash for", "use grep to", "use glob for",
    "always read before", "don't use write for", "the right tool for",
];

const STANDARD_SIGNALS: &[&str] = &[
    "all code should", "every function must", "naming convention",
    "code style", "always include tests", "documentation required",
    "error handling pattern", "logging standard",
];

// ---------------------------------------------------------------------------
// Main extraction function
// ---------------------------------------------------------------------------

/// Extract learnings from a completed session and route to the right brain.
pub async fn extract_and_route(
    messages: &[ToolChatMessage],
    brain_ctx: &BrainContext,
    session_id: &str,
) -> LearningReport {
    let mut report = LearningReport::default();

    // Extract candidates from the conversation
    let candidates = extract_candidates(messages, brain_ctx);
    report.total_candidates = candidates.len();

    if candidates.is_empty() {
        return report;
    }

    // Route each candidate to the right brain
    for candidate in &candidates {
        let sink = route_candidate(candidate, brain_ctx);
        let entry = MemoryEntry {
            id: Uuid::new_v4().to_string(),
            category: candidate.category,
            content: candidate.content.clone(),
            relevance: candidate.confidence,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            source_session: Some(session_id.to_string()),
            tags: Vec::new(),
        };

        match &sink {
            LearningSink::ProjectMemory { project_root } => {
                if write_to_project(project_root, &entry).await.is_ok() {
                    report.project_count += 1;
                }
            }
            LearningSink::FleetBrain => {
                if write_to_brain(&entry).await.is_ok() {
                    report.brain_count += 1;
                }
            }
            LearningSink::HiveMind => {
                if write_to_hive(&entry).await.is_ok() {
                    report.hive_count += 1;
                }
            }
        }
    }

    if report.project_count + report.brain_count + report.hive_count > 0 {
        info!(
            project = report.project_count,
            brain = report.brain_count,
            hive = report.hive_count,
            "auto-learned from session"
        );
    }

    report
}

// ---------------------------------------------------------------------------
// Candidate extraction
// ---------------------------------------------------------------------------

fn extract_candidates(messages: &[ToolChatMessage], brain_ctx: &BrainContext) -> Vec<LearningCandidate> {
    let mut candidates = Vec::new();
    let project_name = brain_ctx.project_name.as_deref().unwrap_or("");

    for msg in messages {
        let role = msg.role.as_str();
        let text = match msg.text_content() {
            Some(t) => t.to_string(),
            None => continue,
        };

        // Only learn from substantive messages
        if text.len() < 20 || text.len() > 5000 {
            continue;
        }

        let lower = text.to_ascii_lowercase();

        // Check each signal category
        for &signal in DECISION_SIGNALS {
            if lower.contains(signal) {
                let excerpt = extract_sentence_around(&text, signal);
                if !excerpt.is_empty() {
                    let is_project = mentions_project(&lower, project_name);
                    candidates.push(LearningCandidate {
                        content: excerpt,
                        category: MemoryCategory::Decision,
                        confidence: if role == "user" { 0.9 } else { 0.7 },
                        is_project_specific: is_project,
                    });
                }
                break; // one extraction per message per category
            }
        }

        for &signal in PREFERENCE_SIGNALS {
            if lower.contains(signal) && role == "user" {
                let excerpt = extract_sentence_around(&text, signal);
                if !excerpt.is_empty() {
                    candidates.push(LearningCandidate {
                        content: excerpt,
                        category: MemoryCategory::Preference,
                        confidence: 0.85,
                        is_project_specific: false, // preferences are usually personal
                    });
                }
                break;
            }
        }

        for &signal in FACT_SIGNALS {
            if lower.contains(signal) {
                let excerpt = extract_sentence_around(&text, signal);
                if !excerpt.is_empty() {
                    candidates.push(LearningCandidate {
                        content: excerpt,
                        category: MemoryCategory::Fact,
                        confidence: 0.7,
                        is_project_specific: mentions_project(&lower, project_name),
                    });
                }
                break;
            }
        }

        for &signal in TOOL_PATTERN_SIGNALS {
            if lower.contains(signal) {
                let excerpt = extract_sentence_around(&text, signal);
                if !excerpt.is_empty() {
                    candidates.push(LearningCandidate {
                        content: excerpt,
                        category: MemoryCategory::ToolPattern,
                        confidence: 0.8,
                        is_project_specific: false,
                    });
                }
                break;
            }
        }

        for &signal in STANDARD_SIGNALS {
            if lower.contains(signal) {
                let excerpt = extract_sentence_around(&text, signal);
                if !excerpt.is_empty() {
                    candidates.push(LearningCandidate {
                        content: excerpt,
                        category: MemoryCategory::CodingStandard,
                        confidence: 0.75,
                        is_project_specific: false,
                    });
                }
                break;
            }
        }
    }

    // Deduplicate by content similarity
    dedup_candidates(&mut candidates);
    candidates
}

fn extract_sentence_around(text: &str, signal: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if let Some(pos) = lower.find(signal) {
        // Find sentence boundaries
        let start = text[..pos].rfind(|c: char| c == '.' || c == '\n' || c == '!' || c == '?')
            .map(|p| p + 1)
            .unwrap_or(0);
        let after = &text[pos..];
        let end = after.find(|c: char| c == '.' || c == '\n' || c == '!' || c == '?')
            .map(|p| pos + p + 1)
            .unwrap_or_else(|| text.len().min(pos + 200));

        let sentence = text[start..end].trim();
        if sentence.len() > 10 && sentence.len() < 500 {
            return sentence.to_string();
        }
    }
    String::new()
}

fn mentions_project(lower_text: &str, project_name: &str) -> bool {
    if project_name.is_empty() { return false; }
    lower_text.contains(&project_name.to_ascii_lowercase())
}

fn dedup_candidates(candidates: &mut Vec<LearningCandidate>) {
    let mut seen_prefixes = std::collections::HashSet::new();
    candidates.retain(|c| {
        // Use first 50 chars as dedup key
        let key = c.content.chars().take(50).collect::<String>().to_ascii_lowercase();
        seen_prefixes.insert(key)
    });
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn route_candidate(candidate: &LearningCandidate, brain_ctx: &BrainContext) -> LearningSink {
    // High-confidence coding standards → Hive Mind (shared with fleet)
    if matches!(candidate.category, MemoryCategory::CodingStandard) && candidate.confidence >= 0.75 {
        return LearningSink::HiveMind;
    }

    // Project-specific content → Project Memory
    if candidate.is_project_specific {
        if let Some(root) = &brain_ctx.project_root {
            return LearningSink::ProjectMemory { project_root: root.clone() };
        }
    }

    // Preferences and tool patterns → Fleet Brain (personal)
    if matches!(candidate.category, MemoryCategory::Preference | MemoryCategory::ToolPattern) {
        return LearningSink::FleetBrain;
    }

    // Decisions in a project context → Project Memory
    if matches!(candidate.category, MemoryCategory::Decision) {
        if let Some(root) = &brain_ctx.project_root {
            return LearningSink::ProjectMemory { project_root: root.clone() };
        }
    }

    // Default → Fleet Brain
    LearningSink::FleetBrain
}

// ---------------------------------------------------------------------------
// Writers — append to entries.json files
// ---------------------------------------------------------------------------

async fn write_to_project(project_root: &Path, entry: &MemoryEntry) -> anyhow::Result<()> {
    let entries_path = project_root.join(".forgefleet").join("memory").join("entries.json");
    write_entry(&entries_path, entry).await
}

async fn write_to_brain(entry: &MemoryEntry) -> anyhow::Result<()> {
    let entries_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("brain")
        .join("learnings.json");
    write_entry(&entries_path, entry).await
}

async fn write_to_hive(entry: &MemoryEntry) -> anyhow::Result<()> {
    let entries_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("hive")
        .join("learnings.json");
    write_entry(&entries_path, entry).await
}

async fn write_entry(path: &Path, entry: &MemoryEntry) -> anyhow::Result<()> {
    // Ensure parent dir exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    // Load existing entries
    let mut entries: Vec<MemoryEntry> = match fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    // Check for duplicates — boost relevance if similar content exists
    let dominated = entries.iter_mut().find(|e| {
        content_similarity(&e.content, &entry.content) > 0.6
    });

    if let Some(existing) = dominated {
        // Boost relevance instead of adding duplicate
        existing.relevance = (existing.relevance + 0.1).min(1.0);
        existing.updated_at = Utc::now();
        debug!(id = %existing.id, "boosted existing entry relevance");
    } else {
        entries.push(entry.clone());
        debug!(category = ?entry.category, "added new learning entry");
    }

    // Write back
    let json = serde_json::to_string_pretty(&entries)?;
    fs::write(path, json).await?;
    Ok(())
}

/// Public helper: write an entry directly to a brain file.
pub async fn apply_entry(path: &std::path::Path, entry: &MemoryEntry) -> anyhow::Result<()> {
    write_entry(path, entry).await
}

/// Simple word-overlap similarity (0.0 to 1.0).
fn content_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> = a.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.len() > 3)
        .collect();
    let words_b: std::collections::HashSet<&str> = b.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.len() > 3)
        .collect();

    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    intersection as f64 / union as f64
}

// ---------------------------------------------------------------------------
// Relevance decay — called periodically to age old entries
// ---------------------------------------------------------------------------

/// Apply relevance decay to all entries in a file.
/// Entries not updated in `days_threshold` get their relevance halved.
pub async fn apply_decay(path: &Path, days_threshold: i64) -> anyhow::Result<usize> {
    let mut entries: Vec<MemoryEntry> = match fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => return Ok(0),
    };

    let now = Utc::now();
    let mut decayed = 0usize;

    for entry in &mut entries {
        let age_days = (now - entry.updated_at).num_days();
        if age_days > days_threshold && entry.relevance > 0.1 {
            entry.relevance *= 0.5;
            entry.updated_at = now;
            decayed += 1;
        }
    }

    // Remove entries with negligible relevance
    let before = entries.len();
    entries.retain(|e| e.relevance > 0.05);
    let pruned = before - entries.len();

    if decayed > 0 || pruned > 0 {
        let json = serde_json::to_string_pretty(&entries)?;
        fs::write(path, json).await?;
        info!(decayed, pruned, path = %path.display(), "applied relevance decay");
    }

    Ok(decayed)
}

/// Run decay across all three brains.
pub async fn decay_all_brains(brain_ctx: &BrainContext) {
    let brain_path = dirs::home_dir().unwrap_or_default()
        .join(".forgefleet").join("brain").join("learnings.json");
    let hive_path = dirs::home_dir().unwrap_or_default()
        .join(".forgefleet").join("hive").join("learnings.json");

    let _ = apply_decay(&brain_path, 30).await;
    let _ = apply_decay(&hive_path, 60).await; // hive decays slower

    if let Some(root) = &brain_ctx.project_root {
        let project_path = root.join(".forgefleet").join("memory").join("entries.json");
        let _ = apply_decay(&project_path, 30).await;
    }
}
