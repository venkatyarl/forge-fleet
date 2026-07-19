//! Basic-memory schema definition.
//!
//! ForgeFleet persists agent memories as markdown notes in the basic-memory
//! style: a YAML frontmatter block describing the memory, a free-form
//! markdown body, and wikilink relations connecting memories into a graph.
//!
//! This module is the source of truth for that schema:
//!
//! - **Frontmatter fields** (in serialized order): `title`, `date`,
//!   `project`, `model`, `tokens`, `tools`, `files_touched`, `type`,
//!   `realm`, `last_updated`.
//! - **Relation linking patterns**: bare `[[Target]]` / `[[Target|alias]]`
//!   wikilinks anywhere in the body, and typed relation lines of the form
//!   `- relation_type [[Target]]`, conventionally listed under a
//!   `## Relations` heading.
//!
//! Only the flat YAML subset emitted by [`BasicMemoryFrontmatter::to_frontmatter`]
//! is supported by [`BasicMemoryFrontmatter::parse`] (scalar `key: value`
//! lines and inline `[a, b]` lists); notes are not expected to carry
//! arbitrary nested YAML.

use serde::{Deserialize, Serialize};

/// Delimiter line opening and closing the YAML frontmatter block.
pub const FRONTMATTER_DELIMITER: &str = "---";

/// Canonical frontmatter fields, in the order they are serialized.
pub const FRONTMATTER_FIELDS: [&str; 10] = [
    "title",
    "date",
    "project",
    "model",
    "tokens",
    "tools",
    "files_touched",
    "type",
    "realm",
    "last_updated",
];

/// Heading under which typed relations are conventionally listed.
pub const RELATIONS_HEADING: &str = "## Relations";

/// Well-known relation types for `- relation_type [[Target]]` lines.
/// Arbitrary relation types are accepted when parsing; these are the ones
/// ForgeFleet emits itself.
pub const RELATION_TYPES: [&str; 6] = [
    "relates_to",
    "part_of",
    "implements",
    "derived_from",
    "supersedes",
    "depends_on",
];

/// Relation type implied by a bare `[[Target]]` wikilink with no typed line.
pub const DEFAULT_RELATION_TYPE: &str = "relates_to";

/// Frontmatter of a basic-memory note.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BasicMemoryFrontmatter {
    /// Human-readable note title.
    pub title: String,
    /// ISO-8601 date or datetime the memory was created.
    pub date: String,
    /// Project the memory belongs to (e.g. `forge-fleet`).
    pub project: String,
    /// Model that produced the memory (e.g. `claude-fable-5`).
    pub model: String,
    /// Tokens spent producing the memory.
    pub tokens: u64,
    /// Tools used during the session that produced the memory.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Repo-relative paths touched during the session.
    #[serde(default)]
    pub files_touched: Vec<String>,
    /// Kind of memory (e.g. `session`, `decision`, `finding`, `reference`).
    #[serde(rename = "type")]
    pub memory_type: String,
    /// Namespace the memory lives in (e.g. `project`, `agent`, `session`).
    pub realm: String,
    /// ISO-8601 date or datetime the memory was last updated.
    pub last_updated: String,
}

impl BasicMemoryFrontmatter {
    /// Serialize to a YAML frontmatter block, delimiters included, fields
    /// in [`FRONTMATTER_FIELDS`] order.
    pub fn to_frontmatter(&self) -> String {
        format!(
            "{d}\n\
             title: {title}\n\
             date: {date}\n\
             project: {project}\n\
             model: {model}\n\
             tokens: {tokens}\n\
             tools: {tools}\n\
             files_touched: {files_touched}\n\
             type: {memory_type}\n\
             realm: {realm}\n\
             last_updated: {last_updated}\n\
             {d}\n",
            d = FRONTMATTER_DELIMITER,
            title = yaml_quote(&self.title),
            date = yaml_quote(&self.date),
            project = yaml_quote(&self.project),
            model = yaml_quote(&self.model),
            tokens = self.tokens,
            tools = yaml_list(&self.tools),
            files_touched = yaml_list(&self.files_touched),
            memory_type = yaml_quote(&self.memory_type),
            realm = yaml_quote(&self.realm),
            last_updated = yaml_quote(&self.last_updated),
        )
    }

    /// Render a full note: frontmatter followed by the markdown body.
    pub fn to_note(&self, body: &str) -> String {
        format!(
            "{}\n{}",
            self.to_frontmatter(),
            body.trim_start_matches('\n')
        )
    }

    /// Parse a note into its frontmatter and body. Returns `None` when the
    /// note has no frontmatter block. Unknown frontmatter keys are ignored;
    /// missing keys keep their `Default` value.
    pub fn parse(note: &str) -> Option<(Self, String)> {
        let (yaml, body) = split_frontmatter(note)?;
        let mut fm = Self::default();
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key.trim() {
                "title" => fm.title = yaml_scalar(value),
                "date" => fm.date = yaml_scalar(value),
                "project" => fm.project = yaml_scalar(value),
                "model" => fm.model = yaml_scalar(value),
                "tokens" => fm.tokens = value.trim().parse().unwrap_or(0),
                "tools" => fm.tools = yaml_list_items(value),
                "files_touched" => fm.files_touched = yaml_list_items(value),
                "type" => fm.memory_type = yaml_scalar(value),
                "realm" => fm.realm = yaml_scalar(value),
                "last_updated" => fm.last_updated = yaml_scalar(value),
                _ => {}
            }
        }
        Some((fm, body.to_string()))
    }
}

/// A typed relation between two memories: `- relation_type [[target]]` or
/// `- relation_type [[target|alias]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub relation_type: String,
    /// Title of the target note.
    pub target: String,
    /// Display alias from `[[target|alias]]`, if any.
    pub alias: Option<String>,
}

impl Relation {
    /// Render as a markdown relation line.
    pub fn to_markdown(&self) -> String {
        match &self.alias {
            Some(alias) => format!("- {} [[{}|{}]]", self.relation_type, self.target, alias),
            None => format!("- {} [[{}]]", self.relation_type, self.target),
        }
    }
}

/// True when `relation_type` is one ForgeFleet emits itself.
pub fn is_known_relation_type(relation_type: &str) -> bool {
    RELATION_TYPES.contains(&relation_type)
}

/// Format a bare wikilink to a target note.
pub fn wikilink(target: &str) -> String {
    format!("[[{target}]]")
}

/// Extract every `[[wikilink]]` target from a markdown body, in order of
/// appearance. `[[target|alias]]` yields only the target.
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("[[") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else {
            break;
        };
        let raw = &after[..close];
        let target = raw.split('|').next().unwrap_or("").trim();
        if !target.is_empty() && !target.contains("[[") {
            links.push(target.to_string());
        }
        rest = &after[close + 2..];
    }
    links
}

/// Extract every typed relation line (`- relation_type [[Target]]`) from a
/// markdown body, regardless of which section it appears in.
pub fn extract_relations(body: &str) -> Vec<Relation> {
    body.lines().filter_map(parse_relation_line).collect()
}

/// Parse a single `- relation_type [[Target]]` / `* relation_type
/// [[Target|alias]]` line. Returns `None` for anything else.
pub fn parse_relation_line(line: &str) -> Option<Relation> {
    let line = line.trim();
    let item = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?;
    let (relation_type, rest) = item.split_once(char::is_whitespace)?;
    let inner = rest.trim().strip_prefix("[[")?;
    let close = inner.find("]]")?;
    let raw = &inner[..close];
    let (target, alias) = match raw.split_once('|') {
        Some((t, a)) => (t.trim(), Some(a.trim()).filter(|a| !a.is_empty())),
        None => (raw.trim(), None),
    };
    if relation_type.is_empty() || target.is_empty() {
        return None;
    }
    Some(Relation {
        relation_type: relation_type.to_string(),
        target: target.to_string(),
        alias: alias.map(str::to_string),
    })
}

/// Split a note into (yaml, body). `None` when no frontmatter block exists.
fn split_frontmatter(note: &str) -> Option<(&str, &str)> {
    let rest = note.trim_start().strip_prefix(FRONTMATTER_DELIMITER)?;
    let close = rest.find("\n---")?;
    let yaml = &rest[..close];
    let body = &rest[close + 4..];
    Some((yaml, body.trim_start_matches('\n')))
}

/// Quote a string as a YAML double-quoted scalar.
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Format a list as an inline YAML flow sequence of quoted scalars.
fn yaml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| yaml_quote(i)).collect();
    format!("[{}]", quoted.join(", "))
}

/// Parse a scalar value, unquoting/unescaping double-quoted strings.
fn yaml_scalar(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for ch in inner.chars() {
            if escaped {
                out.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        raw.to_string()
    }
}

/// Parse an inline `[a, b]` flow sequence, splitting on commas outside quotes.
fn yaml_list_items(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    let Some(inner) = raw.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return if raw.is_empty() {
            Vec::new()
        } else {
            vec![yaml_scalar(raw)]
        };
    };
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                current.push(ch);
                in_quotes = !in_quotes;
            }
            ',' if !in_quotes => items.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        items.push(current);
    }
    items
        .iter()
        .map(|i| yaml_scalar(i))
        .filter(|i| !i.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BasicMemoryFrontmatter {
        BasicMemoryFrontmatter {
            title: "Merge-drain race fix".to_string(),
            date: "2026-07-19".to_string(),
            project: "forge-fleet".to_string(),
            model: "claude-fable-5".to_string(),
            tokens: 12_345,
            tools: vec!["Bash".to_string(), "Edit".to_string()],
            files_touched: vec!["crates/ff-agent/src/merge_drain.rs".to_string()],
            memory_type: "decision".to_string(),
            realm: "project".to_string(),
            last_updated: "2026-07-19T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn frontmatter_round_trips() {
        let fm = sample();
        let note = fm.to_note("Body text.\n\n## Relations\n- relates_to [[Async Merge]]\n");
        let (parsed, body) = BasicMemoryFrontmatter::parse(&note).expect("frontmatter parses");
        assert_eq!(parsed, fm);
        assert!(body.starts_with("Body text."));
        assert!(body.contains(RELATIONS_HEADING));
    }

    #[test]
    fn frontmatter_fields_serialized_in_order() {
        let block = sample().to_frontmatter();
        let mut last = 0;
        for field in FRONTMATTER_FIELDS {
            let pos = block
                .find(&format!("\n{field}: "))
                .or_else(|| block.find(&format!("{field}: ")))
                .unwrap_or_else(|| panic!("field {field} missing from frontmatter"));
            assert!(pos >= last, "field {field} out of order");
            last = pos;
        }
    }

    #[test]
    fn quoting_round_trips_special_chars() {
        let mut fm = sample();
        fm.title = r#"He said "hi" \ bye"#.to_string();
        fm.tools = vec!["a,b".to_string(), r#"c"d"#.to_string()];
        let note = fm.to_note("body");
        let (parsed, _) = BasicMemoryFrontmatter::parse(&note).expect("parses");
        assert_eq!(parsed, fm);
    }

    #[test]
    fn parse_without_frontmatter_returns_none() {
        assert!(BasicMemoryFrontmatter::parse("just a body").is_none());
    }

    #[test]
    fn extracts_wikilinks_and_aliases() {
        let body = "See [[Alpha]] and [[Beta|the beta note]], not [broken] or [[]].";
        assert_eq!(extract_wikilinks(body), vec!["Alpha", "Beta"]);
    }

    #[test]
    fn extracts_typed_relations() {
        let body = "\
## Relations
- relates_to [[Alpha]]
- part_of [[Beta|B]]
* depends_on [[Gamma]]
- plain bullet without a link
- [[bare wikilink bullet]]
";
        let relations = extract_relations(body);
        assert_eq!(
            relations,
            vec![
                Relation {
                    relation_type: "relates_to".to_string(),
                    target: "Alpha".to_string(),
                    alias: None,
                },
                Relation {
                    relation_type: "part_of".to_string(),
                    target: "Beta".to_string(),
                    alias: Some("B".to_string()),
                },
                Relation {
                    relation_type: "depends_on".to_string(),
                    target: "Gamma".to_string(),
                    alias: None,
                },
            ]
        );
        assert!(
            relations
                .iter()
                .all(|r| is_known_relation_type(&r.relation_type))
        );
    }

    #[test]
    fn relation_renders_to_markdown() {
        let line = "- supersedes [[Old Note|old]]";
        let relation = parse_relation_line(line).expect("parses");
        assert_eq!(relation.to_markdown(), line);
        assert_eq!(wikilink("Old Note"), "[[Old Note]]");
    }
}
