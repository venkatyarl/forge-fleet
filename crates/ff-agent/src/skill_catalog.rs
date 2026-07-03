//! Skill catalog — discovers Claude-Code-style `SKILL.md` files at agent
//! session start and injects a catalog block into the system prompt so
//! the agent can self-route based on the user's prompt.
//!
//! Operator directive 2026-04-30 (after seeing Claude Code dynamically
//! pick the right skill mid-conversation): "I want ff to use the skills
//! or tools at the runtime based on what is going on."
//!
//! Skill format (from Claude Code; matched by Open Design):
//!
//! ```text
//! ./<skill_id>/SKILL.md:
//! ---
//! name: critique
//! description: |
//!   Run a 5-dimension expert design review on any HTML artifact ...
//! triggers:
//!   - "critique"
//!   - "design review"
//!   - "audit my design"
//! ---
//!
//! # body - full instructions for the agent
//! ```
//!
//! The catalog parses just the frontmatter (name, description, triggers,
//! optional `od` metadata block) and emits a compact summary; the agent
//! reads the full SKILL.md via the Read tool when it picks one.
//!
//! Discovery roots, in priority order (highest wins on id collision):
//!   1. `<cwd>/.claude/skills/*/SKILL.md`     — project-private
//!   2. `<cwd>/skills/*/SKILL.md`             — project-declared
//!   3. `~/.claude/skills/*/SKILL.md`         — user-global
//!   4. `~/.forgefleet/sub-agents/sub-agent-0/open-design/skills/*/SKILL.md` — fleet-installed (V65)
//!
//! Bounded breadth: at most 256 skills loaded; each skill's body is
//! NOT included in the prompt (just frontmatter), so 256 entries cap
//! at ~20-40 KB. Agent loads the full SKILL.md on demand via Read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sqlx::PgPool;
use tracing::{debug, warn};

const MAX_SKILLS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 400;

/// One scan root, either loaded from V69 `skill_sources` or constructed
/// from the legacy hardcoded defaults.
#[derive(Debug, Clone)]
pub struct SkillSource {
    pub id: String,
    pub label: String,
    /// Pre-expansion path (may contain `$HOME` / `$CWD` / `~/`).
    pub path_template: String,
    pub priority: i32,
}

/// One skill discovered on disk.
#[derive(Debug, Clone)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub source_path: PathBuf,
    pub source_root: String,
}

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
}

/// Default scan roots used when V69 `skill_sources` is unreachable
/// (test, DB down, pre-V69 deployment). Same four locations the
/// hardcoded V68 implementation walked.
fn legacy_default_sources() -> Vec<SkillSource> {
    vec![
        SkillSource {
            id: "project-private".into(),
            label: "project-private (.claude/skills)".into(),
            path_template: "$CWD/.claude/skills".into(),
            priority: 110,
        },
        SkillSource {
            id: "project-declared".into(),
            label: "project-declared (skills/)".into(),
            path_template: "$CWD/skills".into(),
            priority: 100,
        },
        SkillSource {
            id: "user-global".into(),
            label: "user-global (~/.claude/skills)".into(),
            path_template: "$HOME/.claude/skills".into(),
            priority: 50,
        },
        SkillSource {
            id: "fleet-open-design".into(),
            label: "fleet-installed open-design skills".into(),
            path_template: "$HOME/.forgefleet/sub-agents/sub-agent-0/open-design/skills".into(),
            priority: 30,
        },
    ]
}

/// Load enabled scan roots from V69 `skill_sources`, sorted by priority
/// (highest first). Returns the legacy default set when the table is
/// unreachable / empty.
pub async fn load_sources(pool: &PgPool) -> Vec<SkillSource> {
    let rows: Result<Vec<(String, String, String, i32)>, sqlx::Error> = sqlx::query_as(
        r#"
        SELECT id, label, path, priority
          FROM skill_sources
         WHERE enabled = true
         ORDER BY priority DESC, id ASC
        "#,
    )
    .fetch_all(pool)
    .await;
    match rows {
        Ok(r) if !r.is_empty() => r
            .into_iter()
            .map(|(id, label, path, priority)| SkillSource {
                id,
                label,
                path_template: path,
                priority,
            })
            .collect(),
        Ok(_) => {
            debug!("skill_catalog: skill_sources empty, using legacy defaults");
            legacy_default_sources()
        }
        Err(e) => {
            debug!(error = %e, "skill_catalog: skill_sources unreachable, using legacy defaults");
            legacy_default_sources()
        }
    }
}

/// Expand `$CWD`, `$HOME`, `${HOME}`, `~/` placeholders in a source path.
fn expand_path(template: &str, working_dir: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = working_dir.to_string_lossy();
    let expanded = template
        .replace("$CWD", &cwd)
        .replace("${CWD}", &cwd)
        .replace("$HOME", &home)
        .replace("${HOME}", &home);
    let expanded = if let Some(rest) = expanded.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        expanded
    };
    PathBuf::from(expanded)
}

/// Walk the registered skill sources (V69 + legacy fallback) and return
/// a deduplicated catalog. Higher-priority source wins on id collision.
///
/// `discover` (sync) uses legacy defaults only — it's the no-DB path used
/// in tests. Production callers go through [`discover_with_pool`].
pub fn discover(working_dir: &Path) -> Vec<Skill> {
    discover_with_sources(working_dir, &legacy_default_sources())
}

/// Production discovery path: load sources from DB, then walk.
pub async fn discover_with_pool(pool: &PgPool, working_dir: &Path) -> Vec<Skill> {
    let sources = load_sources(pool).await;
    discover_with_sources(working_dir, &sources)
}

fn discover_with_sources(working_dir: &Path, sources: &[SkillSource]) -> Vec<Skill> {
    let roots: Vec<(PathBuf, String)> = sources
        .iter()
        .map(|s| (expand_path(&s.path_template, working_dir), s.label.clone()))
        .collect();

    let mut by_id: HashMap<String, Skill> = HashMap::new();
    for (root, label) in roots {
        if !root.is_dir() {
            continue;
        }
        // Walk recursively up to MAX_DEPTH levels — older layouts use
        // `<root>/<skill>/SKILL.md` (depth=1), our V105 materializer
        // writes `<root>/<source>/<family>/<skill>/SKILL.md` (depth=3).
        // Cap depth so we don't accidentally scan giant trees.
        const MAX_DEPTH: usize = 4;
        let manifests = collect_skill_manifests(&root, MAX_DEPTH);
        for manifest in manifests {
            let skill_dir = match manifest.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let id = skill_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() || by_id.contains_key(&id) {
                continue; // higher-priority root wins
            }
            match parse_skill(&manifest, &id, &label) {
                Some(skill) => {
                    by_id.insert(id, skill);
                }
                None => {
                    debug!(path = %manifest.display(), "skill_catalog: parse failed");
                }
            }
            if by_id.len() >= MAX_SKILLS {
                warn!("skill_catalog: hit MAX_SKILLS={MAX_SKILLS}, truncating");
                break;
            }
        }
    }

    let mut out: Vec<Skill> = by_id.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn parse_skill(manifest: &Path, id: &str, root_label: &str) -> Option<Skill> {
    let raw = std::fs::read_to_string(manifest).ok()?;
    // Frontmatter: between the first `---` line and the next `---` line.
    let trimmed = raw.trim_start();
    let stripped = trimmed.strip_prefix("---")?;
    let after_first = stripped.strip_prefix('\n').unwrap_or(stripped);
    let end = after_first.find("\n---")?;
    let yaml = &after_first[..end];

    let fm: Frontmatter = serde_yaml::from_str(yaml).ok()?;
    let name = fm.name.unwrap_or_else(|| id.to_string());
    let description_raw = fm.description.unwrap_or_default();
    let description = truncate_str(&description_raw, MAX_DESCRIPTION_CHARS);
    Some(Skill {
        id: id.to_string(),
        name,
        description,
        triggers: fm.triggers,
        source_path: manifest.to_path_buf(),
        source_root: root_label.to_string(),
    })
}

/// Collect every `SKILL.md` path under `root` up to `max_depth` levels
/// deep. Depth 1 means `<root>/<name>/SKILL.md`; depth 3 also catches
/// `<root>/<source>/<family>/<name>/SKILL.md` (our V105 layout).
/// Returns paths sorted so deterministic priority resolution sees
/// shallower matches first (rare-case dedupe — `by_id.contains_key`
/// is the real dedupe).
fn collect_skill_manifests(root: &std::path::Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(f) => f,
                Err(_) => continue,
            };
            if ft.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.eq_ignore_ascii_case("SKILL.md"))
                    .unwrap_or(false)
            {
                out.push(path);
            } else if ft.is_dir() && depth < max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    out.sort();
    out
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let cleaned = s.trim().replace('\n', " ");
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max_chars {
        let truncated: String = collapsed.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

/// Render the catalog block ff prepends to the system prompt. Empty
/// input returns an empty string (no block injected).
pub fn render_catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Skills available on this machine (auto-discovered by ForgeFleet)\n\n\
         The following SKILL.md files are present locally. When a skill's \
         description or triggers match the user's prompt, READ its SKILL.md \
         file (path in parentheses) FIRST, then follow its instructions. \
         If no skill applies, continue with your default behavior.\n\n",
    );
    // BUDGET CAP: the fleet has ~700 materialized skills; listing every one
    // injected ~170K chars (~28K tokens) into EVERY system prompt and filled
    // the whole context window on turn 1, so even a one-file `ff run` overflowed.
    // List the highest-priority skills (input is already priority-ordered) up to
    // a budget; the agent reads a full SKILL.md on demand and the trailing note
    // tells it more exist. This is THE fix for the local-backend ctx-overflow.
    const CATALOG_BUDGET_CHARS: usize = 8000;
    let header_len = out.len();
    let mut listed = 0usize;
    for sk in skills {
        let triggers = if sk.triggers.is_empty() {
            String::new()
        } else {
            format!(
                " · triggers: {}",
                sk.triggers
                    .iter()
                    .take(8)
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let entry = format!(
            "- **`{id}`** — {name}{triggers}\n  {desc}\n  ({path}, source: {root})\n",
            id = sk.id,
            name = sk.name,
            triggers = triggers,
            desc = sk.description,
            path = sk.source_path.display(),
            root = sk.source_root,
        );
        // Always list at least one; stop once the budget is exceeded.
        if listed > 0 && out.len() + entry.len() > header_len + CATALOG_BUDGET_CHARS {
            break;
        }
        out.push_str(&entry);
        listed += 1;
    }
    let remaining = skills.len().saturating_sub(listed);
    if remaining > 0 {
        out.push_str(&format!(
            "- … and {remaining} more skill(s) not listed (kept out of the prompt to \
             preserve context). Search the skill roots above or ask if you need one.\n"
        ));
    }
    out.push_str("\n---\n\n");
    out
}

/// Convenience: discover + render in one call. Returns an empty string
/// when no skills exist. Uses the legacy hardcoded source list — for
/// the DB-aware path (V69), use [`catalog_for_with_pool`].
pub fn catalog_for(working_dir: &Path) -> String {
    let skills = discover(working_dir);
    render_catalog(&skills)
}

/// V69 production path: load sources from `skill_sources`, walk, render.
pub async fn catalog_for_with_pool(pool: &PgPool, working_dir: &Path) -> String {
    let skills = discover_with_pool(pool, working_dir).await;
    render_catalog(&skills)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_collapses_whitespace_and_clamps() {
        let s = "  hello\n  world  with    spaces  ";
        assert_eq!(truncate_str(s, 100), "hello world with spaces");
        let long = "a ".repeat(300);
        let out = truncate_str(&long, 50);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn render_catalog_caps_huge_skill_lists() {
        // ~700 skills must NOT inject ~170K chars into the system prompt.
        let skills: Vec<Skill> = (0..700)
            .map(|i| Skill {
                id: format!("skill-{i}"),
                name: format!("Skill number {i}"),
                description:
                    "A reasonably long description of what this skill does, repeated to take space."
                        .into(),
                triggers: vec![format!("trig-{i}")],
                source_path: PathBuf::from(format!("/root/skill-{i}/SKILL.md")),
                source_root: "test".into(),
            })
            .collect();
        let out = render_catalog(&skills);
        // Capped well under the old ~170K; budget is 8K + header/footer.
        assert!(
            out.len() < 12_000,
            "catalog not capped: {} chars",
            out.len()
        );
        // The "+N more" note is present, and the first skill is listed.
        assert!(out.contains("more skill(s) not listed"));
        assert!(out.contains("skill-0"));
        // Empty list still yields nothing.
        assert!(render_catalog(&[]).is_empty());
    }

    #[test]
    fn render_catalog_empty_returns_empty_string() {
        assert_eq!(render_catalog(&[]), "");
    }

    #[test]
    fn render_catalog_includes_id_name_path() {
        let s = Skill {
            id: "critique".into(),
            name: "Critique Skill".into(),
            description: "Run a 5-dim review.".into(),
            triggers: vec!["critique".into(), "design review".into()],
            source_path: PathBuf::from("/tmp/x/skills/critique/SKILL.md"),
            source_root: "test".into(),
        };
        let r = render_catalog(&[s]);
        assert!(r.contains("`critique`"));
        assert!(r.contains("Critique Skill"));
        assert!(r.contains("/tmp/x/skills/critique/SKILL.md"));
        assert!(r.contains("`critique`, `design review`"));
    }
}
