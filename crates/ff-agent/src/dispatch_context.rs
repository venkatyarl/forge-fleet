//! Cortex context pack for Pillar-4 dispatch.
//!
//! Before handing a work_item to a coding agent (codex/claude/kimi), pull the
//! EXACT existing symbols it will need to touch from the Cortex code graph and
//! prepend them to the prompt. Without this the agent grep-storms the whole repo
//! cold to orient itself — burning context + wall-clock, and (on Rust) dragging
//! in the cold-compile explore phase. The graph is shared + indexed once, so this
//! is the "many computers, many models" lever: every LLM on every node starts
//! from the same precise, token-cheap context instead of re-deriving it.
//!
//! v2 prefers loading the precomputed context (`brain_node_ids` and `touched_paths`)
//! stored on the canonical `work_items` row. Those fields are populated by the
//! `work_item_context` extractor during Cortex reindex, so dispatch no longer
//! recomputes the symbol set on every build. If the row has no stored context,
//! the legacy SUBSTRING path over `ff cortex find` is used as a fail-open fallback.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Tokens too generic to be worth a graph lookup even if they look like idents.
const STOPWORDS: &[&str] = &[
    "String", "Result", "Option", "Vec", "Self", "None", "Some", "true", "false", "add", "the",
    "and", "for", "with", "when", "else", "must", "compile", "under", "print", "test", "tests",
    "handler", "value", "count", "code", "rows", "run",
];

/// Extract candidate code identifiers from a task's title+description: CamelCase
/// types (`FleetCommand`) and snake_case names (`sub_agent_count`, `fleet_workers`).
/// Deduped, order-preserving, filtered by length + a small stopword set.
pub fn extract_task_identifiers(title: &str, description: &str) -> Vec<String> {
    let text = format!("{title}\n{description}");
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut token = String::new();
    let flush = |tok: &mut String, out: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        let t = std::mem::take(tok);
        if t.len() < 4 || STOPWORDS.contains(&t.as_str()) {
            return;
        }
        let has_underscore = t.contains('_');
        let has_inner_upper = t.chars().skip(1).any(|c| c.is_ascii_uppercase());
        // Only identifier-shaped tokens: CamelCase or snake_case, not plain words.
        if (has_underscore || has_inner_upper) && seen.insert(t.clone()) {
            out.push(t);
        }
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut out, &mut seen);
        }
    }
    flush(&mut token, &mut out, &mut seen);
    out
}

/// One `ff cortex` invocation in `repo_path`, returning parsed JSON or None.
fn cortex_json(repo_path: &Path, args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new("ff")
        .arg("cortex")
        .args(args)
        .arg("--format")
        .arg("json")
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Make a corpus file path node-independent: the graph stores the LEADER's
/// absolute paths (`/Users/venkat/projects/forge-fleet/crates/...`), which don't
/// exist on a follower. Strip to a repo-relative form so the pointer is valid on
/// any node. Falls back to the basename for non-`crates/` layouts.
fn relativize(file: &str) -> &str {
    if let Some(i) = file.find("/crates/") {
        return &file[i + 1..];
    }
    if let Some(i) = file.find("/src/") {
        return &file[i + 1..];
    }
    file.rsplit('/').next().unwrap_or(file)
}

/// Build the context pack: `--all` substring-find each task identifier across
/// EVERY indexed corpus (cwd-independent — a fresh worktree has no corpus of its
/// own), rank unique hits by fan-in, and emit them as SYMBOL POINTERS
/// (`qualified_name — kind at file:line`). Pointers alone kill the grep-storm:
/// the agent opens the exact symbol directly instead of hunting for it. Returns
/// empty when Cortex has nothing (or is unavailable) — caller prepends it, so
/// empty == unchanged behaviour. Bounded + best-effort.
pub fn build_cortex_context_pack(
    title: &str,
    description: &str,
    repo_path: &Path,
    max_symbols: usize,
) -> String {
    let idents = extract_task_identifiers(title, description);
    if idents.is_empty() {
        return String::new();
    }

    // Unique hits: (qualified_name, kind, relfile, line, fan_in).
    let mut ranked: Vec<(String, String, String, i64, i64)> = Vec::new();
    let mut seen = BTreeSet::new();
    for id in idents.iter().take(8) {
        let Some(serde_json::Value::Array(hits)) =
            cortex_json(repo_path, &["find", id, "--all-corpora"])
        else {
            continue;
        };
        for h in hits.iter().take(3) {
            let Some(qn) = h.get("qualified_name").and_then(|v| v.as_str()) else {
                continue;
            };
            if !seen.insert(qn.to_string()) {
                continue;
            }
            let kind = h
                .get("node_type")
                .and_then(|v| v.as_str())
                .unwrap_or("symbol")
                .trim_start_matches("code:")
                .to_string();
            let file = h.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            let line = h.get("start_line").and_then(|v| v.as_i64()).unwrap_or(0);
            let fan = h.get("fan_in").and_then(|v| v.as_i64()).unwrap_or(0);
            ranked.push((
                qn.to_string(),
                kind,
                relativize(file).to_string(),
                line,
                fan,
            ));
        }
    }
    if ranked.is_empty() {
        return String::new();
    }
    ranked.sort_by(|a, b| b.4.cmp(&a.4)); // highest fan-in first

    let mut pack = String::from(
        "## Relevant existing code (from the Cortex code graph)\n\
         These are the exact symbols this task touches — open them directly \
         instead of grepping the repo to find them:\n\n",
    );
    for (qn, kind, file, line, _) in ranked.into_iter().take(max_symbols) {
        pack.push_str(&format!("- `{qn}` — {kind} at {file}:{line}\n"));
    }
    pack.push('\n');
    pack
}

/// Async wrapper: runs the (subprocess-heavy) pack build off the runtime with a
/// hard cap so a slow/hung Cortex can never stall a dispatch. Fail-open.
pub async fn cortex_context_pack_async(
    title: String,
    description: String,
    repo_path: std::path::PathBuf,
    max_symbols: usize,
) -> String {
    let fut = tokio::task::spawn_blocking(move || {
        build_cortex_context_pack(&title, &description, &repo_path, max_symbols)
    });
    match tokio::time::timeout(Duration::from_secs(20), fut).await {
        Ok(Ok(pack)) => pack,
        _ => String::new(),
    }
}

/// Build a context pack from the precomputed `brain_node_ids` and `touched_paths`
/// stored on the `work_items` row. Deduplicates across the two sources and emits
/// the same "symbol pointer" style as [`build_cortex_context_pack`] so the agent
/// can open the relevant files/symbols directly.
pub fn build_context_pack_from_store(
    brain_node_ids: &[String],
    touched_paths: &[String],
    max_symbols: usize,
) -> String {
    #[derive(Debug)]
    struct Entry {
        name: String,
        kind: String,
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen = BTreeSet::new();

    for path in brain_node_ids.iter().filter(|p| !p.trim().is_empty()) {
        if !seen.insert(path.clone()) {
            continue;
        }
        let entry = if let Some(rest) = path.strip_prefix("code://") {
            if let Some((file, symbol)) = rest.rsplit_once('/') {
                Entry {
                    name: symbol.to_string(),
                    kind: format!("symbol at {file}"),
                }
            } else {
                Entry {
                    name: path.clone(),
                    kind: "symbol".to_string(),
                }
            }
        } else {
            Entry {
                name: path.clone(),
                kind: "brain node".to_string(),
            }
        };
        entries.push(entry);
        if entries.len() >= max_symbols {
            break;
        }
    }

    for path in touched_paths.iter().filter(|p| !p.trim().is_empty()) {
        if !seen.insert(path.clone()) {
            continue;
        }
        entries.push(Entry {
            name: path.clone(),
            kind: "file".to_string(),
        });
        if entries.len() >= max_symbols {
            break;
        }
    }

    if entries.is_empty() {
        return String::new();
    }

    let mut pack = String::from(
        "## Relevant existing code (from the Cortex code graph)\n\
         These are the exact symbols this task touches — open them directly \
         instead of grepping the repo to find them:\n\n",
    );
    for entry in entries {
        pack.push_str(&format!("- `{}` — {}\n", entry.name, entry.kind));
    }
    pack.push('\n');
    pack
}

/// Build a context pack for dispatch, preferring the precomputed DB context and
/// falling back to a live Cortex lookup only when nothing is stored. This keeps
/// dispatch fast and consistent across the fleet while remaining compatible with
/// work items that have not yet been indexed.
pub async fn context_pack_for_dispatch(
    brain_node_ids: Vec<String>,
    touched_paths: Vec<String>,
    title: String,
    description: String,
    repo_path: std::path::PathBuf,
    max_symbols: usize,
) -> String {
    let store_pack = build_context_pack_from_store(&brain_node_ids, &touched_paths, max_symbols);
    if !store_pack.is_empty() {
        return store_pack;
    }
    cortex_context_pack_async(title, description, repo_path, max_symbols).await
}

#[cfg(test)]
mod tests {
    use super::{build_context_pack_from_store, extract_task_identifiers, relativize};

    #[test]
    fn extracts_camel_and_snake_idents_skips_plain_words() {
        let ids = extract_task_identifiers(
            "Add ff fleet set-slots verb",
            "Add a subcommand under the FleetCommand enum. Run \
             UPDATE fleet_workers SET sub_agent_count = $1 WHERE worker_name = $2. \
             Print how many rows changed.",
        );
        assert!(ids.contains(&"FleetCommand".to_string()));
        assert!(ids.contains(&"fleet_workers".to_string()));
        assert!(ids.contains(&"sub_agent_count".to_string()));
        assert!(ids.contains(&"worker_name".to_string()));
        // Plain english words and stopwords are not identifiers.
        assert!(!ids.iter().any(|i| i == "subcommand" || i == "changed"));
        assert!(!ids.contains(&"count".to_string())); // stopword
        // Deduped.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn relativize_strips_absolute_paths_to_repo_relative() {
        // 1. /crates/ found -> strips to repo-relative
        assert_eq!(
            relativize("/Users/venkat/projects/forge-fleet/crates/ff-agent/src/foo.rs"),
            "crates/ff-agent/src/foo.rs"
        );
        // 2. /src/ found (but not /crates/) -> strips to src-relative
        assert_eq!(relativize("/home/x/repo/src/bar.rs"), "src/bar.rs");
        // 3. neither found -> returns basename
        assert_eq!(relativize("/var/log/system/thing.log"), "thing.log");
    }

    #[test]
    fn build_context_pack_from_store_renders_symbols_and_files() {
        let pack = build_context_pack_from_store(
            &[
                "code://crates/ff-agent/src/work_item_dispatch.rs/run_git".to_string(),
                "pm://work_item/82cd7aa9-9942-4774-bdd1-5ac1b3d65c62".to_string(),
            ],
            &["crates/ff-agent/src/dispatch_context.rs".to_string()],
            8,
        );
        assert!(pack.contains("run_git"));
        assert!(pack.contains("crates/ff-agent/src/work_item_dispatch.rs"));
        assert!(pack.contains("crates/ff-agent/src/dispatch_context.rs"));
        assert!(pack.contains("pm://work_item/82cd7aa9-9942-4774-bdd1-5ac1b3d65c62"));
    }

    #[test]
    fn build_context_pack_from_store_deduplicates_and_caps() {
        let pack = build_context_pack_from_store(
            &[
                "code://crates/a.rs/foo".to_string(),
                "code://crates/a.rs/foo".to_string(),
            ],
            &[],
            1,
        );
        // Deduplication keeps one symbol; cap keeps only the first entry.
        assert_eq!(pack.matches("`foo`").count(), 1);
        assert!(!pack.contains("crates/b.rs"));
    }

    #[test]
    fn build_context_pack_from_store_empty_when_no_context() {
        assert!(build_context_pack_from_store(&[], &[], 8).is_empty());
        assert!(
            build_context_pack_from_store(&["".to_string()], &["  ".to_string()], 8).is_empty()
        );
    }
}
