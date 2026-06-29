//! `ff instructions sync` — fallback layer for the ff-means-methodology default
//! (capability roadmap D + the council's recommended fallback to #658's MCP
//! instructions).
//!
//! The forgefleet MCP server `instructions` (#658) deliver the methodology to
//! every TUI at connect — that's the primary path. This is the belt-and-braces
//! fallback for when the MCP server is down or a TUI deprioritizes server
//! instructions: a single canonical methodology block, materialized idempotently
//! into each node's GLOBAL config files (`~/.claude/CLAUDE.md`,
//! `~/.codex/AGENTS.md`, `~/.kimi/AGENTS.md`) plus a shared
//! `~/.forgefleet/instructions/ff-methodology.md`. Defined ONCE here so it stays
//! DRY; every node self-syncs on daemon start, so "distribute to all 15" needs
//! no SSH fan-out.

use std::path::PathBuf;

const START_MARKER: &str = "<!-- ff-build-methodology -->";
const END_MARKER: &str = "<!-- /ff-build-methodology -->";

/// The canonical methodology block (markers included). Single source of truth
/// for the fallback config files; keep in spirit with the MCP `instructions`
/// directive in `ff-mcp/src/server.rs`.
pub const FF_METHODOLOGY_BLOCK: &str = r#"<!-- ff-build-methodology -->
## ForgeFleet build methodology (DEFAULT for every terminal: Claude Code, Codex, Kimi)

When the operator says `ff` or `the fleet`, it means: for any substantive
build/code work in a ForgeFleet-related project, build **with the whole fleet and
all its LLMs**, not solo on one machine:

1. **Build with all the computers + their sub-agents.** Route real work through
   ForgeFleet's distributed build (Pillar-4: `ff pm` work_items -> scheduler ->
   sub-agent worktrees on every computer, under `~/.forgefleet/sub-agents/sub-agent-N/`).
   Don't build everything on one box when the fleet can parallelize it.
2. **Use ALL the LLMs (Hybrid LLM Architecture).** A sub-agent is an orchestrator,
   not bound to its host's RAM -- it can call ANY available LLM: a local model on
   another node (tiered cascade via `fleet_run` / capability router / `ff offload`),
   OR a cloud CLI (claude/codex/kimi/gemini/grok) on its own machine. Cheapest
   capable first.
3. **Use the LLM Council for hard decisions.** Run `ff council --members codex,kimi`
   (multi-LLM consensus) before committing to a non-trivial design.
4. **Use ALL of ff's resources.** Prefer the `forgefleet` MCP tools + ff skills +
   ff agents over generic primitives (`fleet_run`/`fleet_crew`, `cortex_*`, `brain_*`,
   `ff offload`/`supervise`/`research`, `ff db query`).
5. **Dogfood ff.** Route work through `ff` verbs (logged to `ff_interactions`). If ff
   lacks a verb, add it. Solo/inline is only for trivial edits or conversational turns.
<!-- /ff-build-methodology -->"#;

/// Idempotently ensure `block` is present in `content`: replace the existing
/// marked region (inclusive of markers) if present, else append the block. Pure
/// so the marker logic is unit-testable.
pub fn upsert_marked_block(content: &str, block: &str) -> String {
    if let (Some(start), Some(end_idx)) = (content.find(START_MARKER), content.find(END_MARKER)) {
        let end = end_idx + END_MARKER.len();
        if start < end {
            let mut out = String::with_capacity(content.len());
            out.push_str(content[..start].trim_end());
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(block);
            let tail = content[end..].trim_start();
            if !tail.is_empty() {
                out.push('\n');
                out.push_str(tail);
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
    }
    // No (valid) existing block — append.
    let mut out = content.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    out
}

fn home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// The global config files this node's TUIs read.
fn global_config_paths(home: &std::path::Path) -> Vec<PathBuf> {
    vec![
        home.join(".claude/CLAUDE.md"),
        home.join(".codex/AGENTS.md"),
        home.join(".kimi/AGENTS.md"),
    ]
}

/// Sync the methodology block into THIS node's global configs + the shared file.
/// Idempotent: re-running replaces the marked block in place. Returns the paths
/// that were written. Creates parent dirs + files as needed.
pub fn sync_local() -> std::io::Result<Vec<String>> {
    let Some(home) = home() else {
        return Err(std::io::Error::other(
            "HOME unset; cannot locate config files",
        ));
    };
    let mut written = Vec::new();

    // Shared canonical file (the @import target / reference copy).
    let shared = home.join(".forgefleet/instructions/ff-methodology.md");
    if let Some(parent) = shared.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&shared, format!("{FF_METHODOLOGY_BLOCK}\n"))?;
    written.push(shared.display().to_string());

    // Each global config: upsert the marked block (create the file if absent).
    for path in global_config_paths(&home) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = upsert_marked_block(&existing, FF_METHODOLOGY_BLOCK);
        if updated != existing {
            std::fs::write(&path, updated)?;
        }
        written.push(path.display().to_string());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_when_absent() {
        let out = upsert_marked_block("# My config\n", FF_METHODOLOGY_BLOCK);
        assert!(out.starts_with("# My config"));
        assert!(out.contains(START_MARKER));
        assert!(out.contains(END_MARKER));
        assert_eq!(out.matches(START_MARKER).count(), 1);
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_marked_block("# cfg\n", FF_METHODOLOGY_BLOCK);
        let twice = upsert_marked_block(&once, FF_METHODOLOGY_BLOCK);
        assert_eq!(once, twice, "re-sync must not duplicate or drift");
        assert_eq!(twice.matches(START_MARKER).count(), 1);
    }

    #[test]
    fn upsert_replaces_stale_block_in_place() {
        let stale = format!("# cfg\n\n{START_MARKER}\nOLD CONTENT\n{END_MARKER}\n\n## keep me\n");
        let out = upsert_marked_block(&stale, FF_METHODOLOGY_BLOCK);
        assert!(!out.contains("OLD CONTENT"));
        assert!(out.contains("Hybrid LLM Architecture"));
        assert!(
            out.contains("## keep me"),
            "content after the block survives"
        );
        assert_eq!(out.matches(START_MARKER).count(), 1);
    }

    #[test]
    fn empty_config_just_gets_the_block() {
        let out = upsert_marked_block("", FF_METHODOLOGY_BLOCK);
        assert!(out.trim_start().starts_with(START_MARKER));
        assert!(out.ends_with('\n'));
    }
}
