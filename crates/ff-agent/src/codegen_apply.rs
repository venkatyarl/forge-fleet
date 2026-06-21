use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use sqlx::PgPool;
use tracing::{info, warn};

const SMALL_CONTEXT_FILE_BYTES: usize = 12_000;
const LARGE_CONTEXT_FILE_CHARS: usize = 16_000;
const REGION_CONTEXT_LINES: usize = 25;
const FALLBACK_LINES: usize = 60;

#[derive(Debug, Clone)]
pub struct CodegenOutcome {
    pub applied: bool,
    pub rounds: u32,
    pub final_diff: Option<String>,
    pub error: Option<String>,
}

pub async fn codegen_apply(
    pool: &PgPool,
    repo_path: &Path,
    task: &str,
    model_hint: Option<&str>,
    max_rounds: u32,
) -> Result<CodegenOutcome> {
    let mut last_edits: Option<String> = None;
    let mut last_error: Option<String> = None;
    let mut rounds = 0;

    for round in 1..=max_rounds {
        rounds = round;
        let prompt = build_prompt(
            repo_path,
            task,
            last_edits.as_deref(),
            last_error.as_deref(),
        )?;
        info!(
            round,
            max_rounds, "requesting codegen edits from fleet model"
        );

        let response = crate::fleet_oneshot::fleet_oneshot(
            pool,
            &prompt,
            model_hint,
            Some(Duration::from_secs(300)),
        )
        .await
        .with_context(|| format!("fleet_oneshot round {round}"))?;

        let edits = match parse_edit_blocks(&response.text) {
            Ok(edits) if !edits.is_empty() => edits,
            Ok(_) => {
                let err = "model response did not contain any edit blocks".to_string();
                warn!(round, error = %err, "codegen response rejected");
                last_edits = None;
                last_error = Some(err);
                continue;
            }
            Err(e) => {
                let err = e.to_string();
                warn!(round, error = %err, "codegen response rejected");
                last_edits = Some(response.text);
                last_error = Some(err);
                continue;
            }
        };
        let edit_summary = format_edit_summary(&edits);

        let snapshots = match apply_edits(repo_path, &edits) {
            Ok(snapshots) => snapshots,
            Err(e) => {
                let err = e.to_string();
                warn!(round, error = %err, "codegen edits failed to apply");
                clean_worktree(repo_path)?;
                last_edits = Some(edit_summary);
                last_error = Some(err);
                continue;
            }
        };

        // Guard against no-op edits: a SEARCH/REPLACE where REPLACE == the matched
        // text (or edits that otherwise change nothing) would pass apply + cargo
        // check and be reported applied:true while the working tree is UNCHANGED
        // (live-observed false-success on a 183K file). Require a real diff.
        let unchanged = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["status", "--porcelain"])
            .output()
            .map(|o| o.stdout.is_empty())
            .unwrap_or(false);
        if unchanged {
            let err =
                "edits applied but produced NO change (no-op SEARCH/REPLACE)".to_string();
            warn!(round, "{}", err);
            last_edits = Some(edit_summary);
            last_error = Some(err);
            continue;
        }

        let check = Command::new("cargo")
            .arg("check")
            .current_dir(repo_path)
            .output()
            .with_context(|| format!("run cargo check in {}", repo_path.display()))?;

        if !check.status.success() {
            let err = command_error("cargo check", &check);
            warn!(round, error = %err, "codegen edits failed cargo check");
            restore_snapshots(&snapshots)?;
            clean_worktree(repo_path)?;
            last_edits = Some(edit_summary);
            last_error = Some(err);
            continue;
        }

        return Ok(CodegenOutcome {
            applied: true,
            rounds,
            final_diff: Some(edit_summary),
            error: None,
        });
    }

    Ok(CodegenOutcome {
        applied: false,
        rounds,
        final_diff: None,
        error: last_error,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edit {
    path: String,
    search: String,
    replace: String,
}

#[derive(Debug)]
struct FileSnapshot {
    path: PathBuf,
    previous: Option<String>,
}

fn build_prompt(
    repo_path: &Path,
    task: &str,
    previous_edits: Option<&str>,
    previous_error: Option<&str>,
) -> Result<String> {
    let mut prompt = format!(
        "Task:\n{task}\n\n\
         Output ONLY one or more SEARCH/REPLACE edit blocks. Do not include prose, explanations, markdown fences, or any text outside edit blocks.\n\
         Each edit block must be EXACTLY in this format:\n\
         *** FILE: <path relative to repo root>\n\
         <<<<<<< SEARCH\n\
         <the exact existing lines to find, copied verbatim from the current file>\n\
         =======\n\
         <the replacement lines>\n\
         >>>>>>> REPLACE\n\n\
         Rules:\n\
         - The SEARCH text must match the current file content EXACTLY, including whitespace.\n\
         - For large files, you are shown only RELEVANT REGIONS with line numbers; SEARCH blocks must match lines shown in those regions EXACTLY.\n\
         - To create a NEW file, leave the SEARCH section empty.\n\
         - To append, SEARCH a unique existing snippet and include it in REPLACE plus the new code.\n\
         - Paths must be relative to the repo root."
    );

    let identifiers = task_identifiers(task);
    for path in task_context_paths(repo_path, task)? {
        let abs = repo_path.join(&path);
        let content =
            fs::read_to_string(&abs).with_context(|| format!("read {}", abs.display()))?;

        if content.len() <= SMALL_CONTEXT_FILE_BYTES {
            prompt.push_str("\n\nCurrent content of ");
            prompt.push_str(&path.to_string_lossy());
            prompt.push_str(":\n");
            prompt.push_str(&content);
        } else {
            prompt.push_str("\n\nRelevant regions of ");
            prompt.push_str(&path.to_string_lossy());
            prompt.push_str(" (large file; not full content):\n");
            prompt.push_str(&regions_with_path_headers(
                &path.to_string_lossy(),
                &extract_relevant_regions(&content, &identifiers),
            ));
        }
    }

    if let Some(edits) = previous_edits {
        prompt.push_str("\n\nPrevious edit blocks that failed:\n");
        prompt.push_str(edits.trim());
    }
    if let Some(error) = previous_error {
        prompt.push_str("\n\nExact failure to fix:\n");
        prompt.push_str(error.trim());
    }

    Ok(prompt)
}

fn task_identifiers(task: &str) -> Vec<String> {
    let stoplist: HashSet<&'static str> = [
        "the",
        "and",
        "for",
        "you",
        "are",
        "but",
        "not",
        "with",
        "this",
        "that",
        "from",
        "into",
        "file",
        "files",
        "function",
        "functions",
        "add",
        "return",
        "value",
        "values",
        "line",
        "lines",
        "task",
        "code",
        "make",
        "must",
        "should",
        "would",
        "could",
        "when",
        "then",
        "than",
        "have",
        "has",
        "had",
        "was",
        "were",
        "will",
        "can",
        "its",
        "your",
        "our",
        "their",
        "there",
        "here",
        "only",
        "also",
        "each",
        "any",
        "all",
        "new",
        "old",
        "use",
        "using",
        "used",
        "set",
        "get",
        "put",
        "let",
        "fn",
        "mod",
        "pub",
        "str",
        "string",
        "true",
        "false",
        "none",
        "some",
        "result",
        "error",
        "path",
        "token",
        "tokens",
        "content",
        "current",
        "existing",
        "large",
        "small",
    ]
    .into_iter()
    .collect();

    let mut identifiers = Vec::new();
    let mut seen = HashSet::new();
    let mut start = None;

    for (idx, ch) in task.char_indices() {
        match start {
            Some(s) if ch.is_ascii_alphanumeric() || ch == '_' => {
                if idx + ch.len_utf8() == task.len() {
                    push_identifier(&task[s..], &stoplist, &mut seen, &mut identifiers);
                }
            }
            Some(s) => {
                push_identifier(&task[s..idx], &stoplist, &mut seen, &mut identifiers);
                start = if ch.is_ascii_alphabetic() || ch == '_' {
                    Some(idx)
                } else {
                    None
                };
            }
            None if ch.is_ascii_alphabetic() || ch == '_' => {
                start = Some(idx);
                if idx + ch.len_utf8() == task.len() {
                    push_identifier(&task[idx..], &stoplist, &mut seen, &mut identifiers);
                }
            }
            None => {}
        }
    }

    identifiers
}

fn push_identifier(
    token: &str,
    stoplist: &HashSet<&'static str>,
    seen: &mut HashSet<String>,
    identifiers: &mut Vec<String>,
) {
    if token.len() < 3 {
        return;
    }
    let ident = token.to_ascii_lowercase();
    if stoplist.contains(ident.as_str()) {
        return;
    }
    if seen.insert(ident.clone()) {
        identifiers.push(ident);
    }
}

fn regions_with_path_headers(path: &str, regions: &str) -> String {
    let mut out = String::with_capacity(regions.len() + path.len());
    for line in regions.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("Region ") {
            out.push_str("Region of ");
            out.push_str(path);
            out.push(' ');
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn extract_relevant_regions(content: &str, identifiers: &[String]) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    if lines.is_empty() {
        return "No content.\n".to_string();
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if !identifiers.is_empty() {
        for (idx, line) in lines.iter().enumerate() {
            let lower = line.to_ascii_lowercase();
            if identifiers
                .iter()
                .any(|identifier| lower.contains(identifier))
            {
                let start = idx.saturating_sub(REGION_CONTEXT_LINES);
                let end = (idx + REGION_CONTEXT_LINES).min(lines.len() - 1);
                if let Some((_, last_end)) = ranges.last_mut()
                    && start <= *last_end + 1
                {
                    *last_end = (*last_end).max(end);
                    continue;
                }
                ranges.push((start, end));
            }
        }
    }

    if ranges.is_empty() {
        return fallback_head_tail_regions(&lines);
    }

    render_ranges(&lines, &ranges)
}

fn fallback_head_tail_regions(lines: &[&str]) -> String {
    let mut ranges = vec![(0, FALLBACK_LINES.min(lines.len()) - 1)];
    if lines.len() > FALLBACK_LINES {
        let tail_start = lines.len().saturating_sub(FALLBACK_LINES);
        if tail_start <= ranges[0].1 + 1 {
            ranges[0].1 = lines.len() - 1;
        } else {
            ranges.push((tail_start, lines.len() - 1));
        }
    }

    let mut out = String::from(
        "No task identifiers matched this large file; showing first and last 60 lines.\n",
    );
    out.push_str(&render_ranges(lines, &ranges));
    out
}

fn render_ranges(lines: &[&str], ranges: &[(usize, usize)]) -> String {
    let mut out = String::new();
    let mut omitted = 0usize;

    for (idx, (start, end)) in ranges.iter().enumerate() {
        let mut block = String::new();
        if idx > 0 {
            block.push('\n');
        }
        block.push_str(&format!("Region (lines {}-{}):\n", start + 1, end + 1));
        for line in &lines[*start..=*end] {
            block.push_str(line);
        }
        if !block.ends_with('\n') {
            block.push('\n');
        }

        if out.len() + block.len() > LARGE_CONTEXT_FILE_CHARS {
            omitted = ranges.len() - idx;
            break;
        }
        out.push_str(&block);
    }

    if omitted > 0 {
        out.push_str(&format!(
            "\n... omitted {omitted} later region(s) after ~{LARGE_CONTEXT_FILE_CHARS} chars for this file.\n"
        ));
    }

    out
}

fn task_context_paths(repo_path: &Path, task: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for raw in task.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '"'
                    | '\''
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | ':'
                    | '!'
                    | '?'
            )
        });
        let token = token.trim_end_matches('.');
        if !token.contains('/') || token.contains("://") {
            continue;
        }
        let Some(last_segment) = token.rsplit('/').next() else {
            continue;
        };
        if !last_segment.contains('.') {
            continue;
        }

        let rel = match normalize_relative_path(token) {
            Some(path) => path,
            None => continue,
        };
        let abs = repo_path.join(&rel);
        if abs.is_file() && seen.insert(rel.clone()) {
            out.push(rel);
        }
    }

    Ok(out)
}

fn parse_edit_blocks(response: &str) -> Result<Vec<Edit>> {
    let mut edits = Vec::new();
    for raw_block in response.split("*** FILE:").skip(1) {
        let (path, body) = raw_block
            .split_once('\n')
            .ok_or_else(|| anyhow!("edit block missing body after FILE line"))?;
        let path = path.trim().to_string();
        if path.is_empty() {
            return Err(anyhow!("edit block has empty FILE path"));
        }

        let body = strip_one_leading_newline(
            body.strip_prefix("<<<<<<< SEARCH")
                .ok_or_else(|| anyhow!("edit block for {path} missing <<<<<<< SEARCH marker"))?,
        );
        let (search, rest) = split_marker_line(body, "=======")
            .ok_or_else(|| anyhow!("edit block for {path} missing ======= marker"))?;
        let rest = strip_one_leading_newline(rest);
        let (replace, tail) = split_marker_line(rest, ">>>>>>> REPLACE")
            .ok_or_else(|| anyhow!("edit block for {path} missing >>>>>>> REPLACE marker"))?;
        if !tail.trim().is_empty() {
            return Err(anyhow!(
                "edit block for {path} has trailing text after REPLACE marker"
            ));
        }

        edits.push(Edit {
            path,
            search: search.to_string(),
            replace: replace.to_string(),
        });
    }

    Ok(edits)
}

fn split_marker_line<'a>(input: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    if let Some(rest) = input.strip_prefix(marker) {
        return Some(("", rest));
    }
    if let Some(pos) = input.find(&format!("\n{marker}")) {
        return Some((&input[..pos + 1], &input[pos + 1 + marker.len()..]));
    }
    if let Some(pos) = input.find(&format!("\r\n{marker}")) {
        return Some((&input[..pos + 2], &input[pos + 2 + marker.len()..]));
    }
    None
}

fn strip_one_leading_newline(input: &str) -> &str {
    input
        .strip_prefix("\r\n")
        .or_else(|| input.strip_prefix('\n'))
        .unwrap_or(input)
}

fn apply_edits(repo_path: &Path, edits: &[Edit]) -> Result<Vec<FileSnapshot>> {
    let mut snapshots = Vec::new();
    let mut snapshotted = HashSet::new();

    for edit in edits {
        let result = apply_one_edit(repo_path, edit, &mut snapshots, &mut snapshotted);
        if let Err(e) = result {
            if let Err(restore_err) = restore_snapshots(&snapshots) {
                warn!(error = %restore_err, "failed to restore codegen edit snapshots");
            }
            return Err(e);
        }
    }

    Ok(snapshots)
}

fn apply_one_edit(
    repo_path: &Path,
    edit: &Edit,
    snapshots: &mut Vec<FileSnapshot>,
    snapshotted: &mut HashSet<PathBuf>,
) -> Result<()> {
    let path = resolve_repo_path(repo_path, &edit.path)?;
    snapshot_file(&path, snapshots, snapshotted)?;

    if edit.search.is_empty() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create parent dirs for {}", path.display()))?;
        }
        fs::write(&path, &edit.replace).with_context(|| format!("write {}", path.display()))?;
        return Ok(());
    }

    let content = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let Some(pos) = content.find(&edit.search) else {
        return Err(anyhow!("SEARCH block not found in {}", edit.path));
    };
    let mut updated = String::with_capacity(content.len() - edit.search.len() + edit.replace.len());
    updated.push_str(&content[..pos]);
    updated.push_str(&edit.replace);
    updated.push_str(&content[pos + edit.search.len()..]);
    fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;

    Ok(())
}

fn snapshot_file(
    path: &Path,
    snapshots: &mut Vec<FileSnapshot>,
    snapshotted: &mut HashSet<PathBuf>,
) -> Result<()> {
    if !snapshotted.insert(path.to_path_buf()) {
        return Ok(());
    }

    let previous = match fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    snapshots.push(FileSnapshot {
        path: path.to_path_buf(),
        previous,
    });
    Ok(())
}

fn restore_snapshots(snapshots: &[FileSnapshot]) -> Result<()> {
    for snapshot in snapshots.iter().rev() {
        match &snapshot.previous {
            Some(content) => {
                fs::write(&snapshot.path, content)
                    .with_context(|| format!("restore {}", snapshot.path.display()))?;
            }
            None => match fs::remove_file(&snapshot.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("remove {}", snapshot.path.display()));
                }
            },
        }
    }
    Ok(())
}

fn resolve_repo_path(repo_path: &Path, path: &str) -> Result<PathBuf> {
    let rel = normalize_relative_path(path)
        .ok_or_else(|| anyhow!("edit path escapes repo root or is not relative: {path}"))?;
    Ok(repo_path.join(rel))
}

fn normalize_relative_path(path: &str) -> Option<PathBuf> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return None;
    }

    let mut rel = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel)
    }
}

fn format_edit_summary(edits: &[Edit]) -> String {
    edits
        .iter()
        .map(|edit| {
            format!(
                "*** FILE: {}\n<<<<<<< SEARCH\n{}=======\n{}>>>>>>> REPLACE",
                edit.path,
                marker_section(&edit.search),
                marker_section(&edit.replace)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn marker_section(text: &str) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

fn clean_worktree(repo_path: &Path) -> Result<()> {
    let revert = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("checkout")
        .arg("--")
        .arg(".")
        .output()
        .with_context(|| format!("revert failed codegen edits in {}", repo_path.display()))?;
    if !revert.status.success() {
        return Err(anyhow!("{}", command_error("git checkout -- .", &revert)));
    }
    Ok(())
}

fn command_error(name: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let code = output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());

    if !stderr.is_empty() {
        format!("{name} failed with exit {code}:\n{stderr}")
    } else if !stdout.is_empty() {
        format!("{name} failed with exit {code}:\n{stdout}")
    } else {
        format!("{name} failed with exit {code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_edit_blocks() {
        let response = "*** FILE: src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n*** FILE: src/main.rs\n<<<<<<< SEARCH\n=======\ncreated\n>>>>>>> REPLACE";

        let edits = parse_edit_blocks(response).unwrap();

        assert_eq!(
            edits,
            vec![
                Edit {
                    path: "src/lib.rs".to_string(),
                    search: "old\n".to_string(),
                    replace: "new\n".to_string(),
                },
                Edit {
                    path: "src/main.rs".to_string(),
                    search: String::new(),
                    replace: "created\n".to_string(),
                },
            ]
        );
    }

    #[test]
    fn rejects_escaping_paths() {
        assert!(normalize_relative_path("../outside.rs").is_none());
        assert!(normalize_relative_path("/tmp/outside.rs").is_none());
        assert_eq!(
            normalize_relative_path("./src/lib.rs").unwrap(),
            PathBuf::from("src/lib.rs")
        );
    }

    #[test]
    fn applies_first_matching_search_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old\nold\n").unwrap();

        apply_edits(
            dir.path(),
            &[Edit {
                path: "src/lib.rs".to_string(),
                search: "old\n".to_string(),
                replace: "new\n".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "new\nold\n");
    }

    #[test]
    fn codegen_extract_relevant_regions_includes_matching_identifier_line() {
        let content = (1..=80)
            .map(|line| {
                if line == 40 {
                    "fn special_handler() {}\n".to_string()
                } else {
                    format!("let line_{line} = {line};\n")
                }
            })
            .collect::<String>();

        let regions = extract_relevant_regions(&content, &["special_handler".to_string()]);

        assert!(regions.contains("Region (lines 15-65):"));
        assert!(regions.contains("fn special_handler() {}\n"));
        assert!(!regions.contains("No task identifiers matched"));
    }

    #[test]
    fn codegen_extract_relevant_regions_falls_back_to_head_and_tail() {
        let content = (1..=140)
            .map(|line| format!("let unrelated_{line} = {line};\n"))
            .collect::<String>();

        let regions = extract_relevant_regions(&content, &["missing_identifier".to_string()]);

        assert!(regions.contains("No task identifiers matched this large file"));
        assert!(regions.contains("Region (lines 1-60):"));
        assert!(regions.contains("let unrelated_1 = 1;\n"));
        assert!(regions.contains("Region (lines 81-140):"));
        assert!(regions.contains("let unrelated_140 = 140;\n"));
    }

    #[test]
    fn restores_created_file_after_failed_later_edit() {
        let dir = tempfile::tempdir().unwrap();

        let err = apply_edits(
            dir.path(),
            &[
                Edit {
                    path: "src/new.rs".to_string(),
                    search: String::new(),
                    replace: "new\n".to_string(),
                },
                Edit {
                    path: "src/missing.rs".to_string(),
                    search: "missing\n".to_string(),
                    replace: "still missing\n".to_string(),
                },
            ],
        )
        .unwrap_err();

        assert!(err.to_string().contains("src/missing.rs"));
        assert!(!dir.path().join("src/new.rs").exists());
    }
}
