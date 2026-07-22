//! Markdown extraction utility — pulls candidate work items out of a
//! Markdown document (checkboxes, `TODO` headers, `Action:` blocks).
//!
//! This is intentionally shallow: it returns the raw matched text per
//! candidate rather than a parsed/structured type, so callers can apply
//! their own derivation (dedup, confidence scoring, project tagging) on
//! top. Uses `regex` (already a workspace dependency) instead of pulling
//! in a full Markdown AST crate.

use regex::Regex;
use std::sync::LazyLock;

static CHECKBOX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[-*+]\s*\[[ xX]\]\s*(.+)$").unwrap());
static TODO_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^#{1,6}\s*todo\b\s*:?\s*(.*)$").unwrap());
static ACTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^action\s*:\s*(.+)$").unwrap());

/// Extract candidate work-item strings from a Markdown document.
///
/// Scans line by line for:
/// - checkbox list items: `- [ ]` / `- [x]`
/// - `TODO` headers: `## TODO`, `### TODO: fix the thing`
/// - `Action:` blocks: `Action: do the thing`
///
/// Returns the raw text of each match (marker stripped, whitespace
/// trimmed) in document order. Empty candidates are skipped.
pub fn extract_candidates(doc: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    for raw_line in doc.lines() {
        let line = strip_prefix_markers(raw_line.trim());
        if line.is_empty() {
            continue;
        }

        if let Some(caps) = CHECKBOX_RE.captures(line) {
            push_if_nonempty(&mut candidates, &caps[1]);
            continue;
        }

        if let Some(caps) = TODO_HEADER_RE.captures(line) {
            let text = caps[1].trim();
            candidates.push(if text.is_empty() {
                line.to_string()
            } else {
                text.to_string()
            });
            continue;
        }

        if let Some(caps) = ACTION_RE.captures(line) {
            push_if_nonempty(&mut candidates, &caps[1]);
        }
    }

    candidates
}

/// Strip a single leading blockquote marker (`> `) so `Action:`/`TODO`
/// blocks quoted inside a blockquote still match.
fn strip_prefix_markers(line: &str) -> &str {
    line.strip_prefix('>').map(str::trim).unwrap_or(line)
}

fn push_if_nonempty(candidates: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        candidates.push(text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_unchecked_and_checked_checkboxes() {
        let doc = "- [ ] write the doc\n- [x] ship the fix\n";
        assert_eq!(
            extract_candidates(doc),
            vec!["write the doc".to_string(), "ship the fix".to_string()]
        );
    }

    #[test]
    fn extracts_todo_headers() {
        let doc = "## TODO: refactor the parser\n### TODO\nfollow-up text\n";
        let candidates = extract_candidates(doc);
        assert_eq!(candidates[0], "refactor the parser");
        assert_eq!(candidates[1], "### TODO");
    }

    #[test]
    fn extracts_action_blocks() {
        let doc = "Some notes.\nAction: rotate the credentials\n> Action: page oncall\n";
        assert_eq!(
            extract_candidates(doc),
            vec![
                "rotate the credentials".to_string(),
                "page oncall".to_string()
            ]
        );
    }

    #[test]
    fn ignores_prose_and_blank_lines() {
        let doc = "\n# Heading\n\nJust a normal paragraph with no markers.\n";
        assert!(extract_candidates(doc).is_empty());
    }

    #[test]
    fn empty_doc_returns_empty_vec() {
        assert!(extract_candidates("").is_empty());
    }
}
