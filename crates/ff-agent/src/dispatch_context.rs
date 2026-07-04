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
//! v1 uses SUBSTRING find on identifiers lifted from the task text (semantic
//! search needs a loaded embedding endpoint, which isn't always up). Fail-OPEN:
//! any Cortex hiccup yields an empty pack and dispatch proceeds exactly as before.

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
    let mut flush = |tok: &mut String, out: &mut Vec<String>, seen: &mut BTreeSet<String>| {
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

/// Build the context pack: substring-find each task identifier in the graph,
/// rank the unique hits by fan-in, and `show` the top `max_symbols` bodies.
/// Returns an empty string when Cortex has nothing (or is unavailable) — the
/// caller prepends it, so empty == unchanged behaviour. Bounded + best-effort.
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

    // Collect unique (qualified_name, fan_in) hits across the identifiers.
    let mut ranked: Vec<(String, i64)> = Vec::new();
    let mut seen = BTreeSet::new();
    for id in idents.iter().take(12) {
        let Some(serde_json::Value::Array(hits)) = cortex_json(repo_path, &["find", id]) else {
            continue;
        };
        for h in hits.iter().take(4) {
            let Some(qn) = h.get("qualified_name").and_then(|v| v.as_str()) else {
                continue;
            };
            if seen.insert(qn.to_string()) {
                let fan = h.get("fan_in").and_then(|v| v.as_i64()).unwrap_or(0);
                ranked.push((qn.to_string(), fan));
            }
        }
    }
    if ranked.is_empty() {
        return String::new();
    }
    // Highest fan-in first — the load-bearing symbols the change orbits.
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    let mut pack = String::from(
        "## Relevant existing code (from the Cortex code graph)\n\
         Start from these exact symbols instead of grepping the whole repo — they \
         are what this task touches or sits next to:\n\n",
    );
    let mut included = 0usize;
    for (qn, _) in ranked {
        if included >= max_symbols {
            break;
        }
        let Some(obj) = cortex_json(repo_path, &["show", &qn, "--max-lines", "45"]) else {
            continue;
        };
        let source = obj.get("source").and_then(|v| v.as_str()).unwrap_or("");
        if source.trim().is_empty() {
            continue;
        }
        let file = obj.get("file").and_then(|v| v.as_str()).unwrap_or("?");
        let start = obj.get("start_line").and_then(|v| v.as_i64()).unwrap_or(0);
        let truncated = obj
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        pack.push_str(&format!(
            "### {qn}  ({file}:{start})\n```\n{}\n```{}\n\n",
            source.trim_end(),
            if truncated { " (truncated)" } else { "" }
        ));
        included += 1;
    }
    if included == 0 {
        return String::new();
    }
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

#[cfg(test)]
mod tests {
    use super::extract_task_identifiers;

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
}
