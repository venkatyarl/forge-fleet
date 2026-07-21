use std::collections::{BTreeSet, HashMap, HashSet};
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
        let rp = repo_path.to_path_buf();
        let task = task.to_string();
        let previous_edits = last_edits.clone();
        let previous_error = last_error.clone();
        let prompt = tokio::task::spawn_blocking(move || {
            build_prompt(
                &rp,
                &task,
                previous_edits.as_deref(),
                previous_error.as_deref(),
            )
        })
        .await
        .map_err(|e| anyhow!("build prompt task panicked: {e}"))??;
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

        let rp = repo_path.to_path_buf();
        let edits_to_apply = edits.clone();
        let snapshots = match tokio::task::spawn_blocking(move || apply_edits(&rp, &edits_to_apply))
            .await
            .map_err(|e| anyhow!("apply edits task panicked: {e}"))?
        {
            Ok(snapshots) => snapshots,
            Err(e) => {
                let err = e.to_string();
                warn!(round, error = %err, "codegen edits failed to apply");
                let rp = repo_path.to_path_buf();
                tokio::task::spawn_blocking(move || clean_worktree(&rp))
                    .await
                    .map_err(|e| anyhow!("clean worktree task panicked: {e}"))??;
                last_edits = Some(edit_summary);
                last_error = Some(err);
                continue;
            }
        };

        // Guard against no-op edits: a SEARCH/REPLACE where REPLACE == the matched
        // text (or edits that otherwise change nothing) would pass apply + cargo
        // check and be reported applied:true while the working tree is UNCHANGED
        // (live-observed false-success on a 183K file). Require a real diff.
        let rp = repo_path.to_path_buf();
        let unchanged = tokio::task::spawn_blocking(move || {
            Command::new("git")
                .arg("-C")
                .arg(rp)
                .args(["status", "--porcelain"])
                .output()
                .map(|o| o.stdout.is_empty())
                .unwrap_or(false)
        })
        .await
        .map_err(|e| anyhow!("git status task panicked: {e}"))?;
        if unchanged {
            let err = "edits applied but produced NO change (no-op SEARCH/REPLACE)".to_string();
            warn!(round, "{}", err);
            last_edits = Some(edit_summary);
            last_error = Some(err);
            continue;
        }

        let rp = repo_path.to_path_buf();
        let edits_for_verify = edits.clone();
        let verify = tokio::task::spawn_blocking(move || {
            let changed_packages = changed_crate_packages(&rp, &edits_for_verify)
                .into_iter()
                .collect::<Vec<_>>();
            verify_command(&rp, &changed_packages)
        })
        .await
        .map_err(|e| anyhow!("select verify command task panicked: {e}"))?;
        if let Some((program, args)) = verify {
            let check_name = format_command(&program, &args);
            // Run the verify subprocess OFF the async runtime. It can take MINUTES
            // (cargo check/build on the changed crates); a blocking
            // Command::output() here runs on the tokio worker thread and starves
            // the dispatch HeartbeatGuard task (same runtime), freezing the lease
            // heartbeat. The scheduler's stale-heartbeat reaper (180s) then
            // reclaims the ACTIVE build as "stalled", burning all 3 attempts on
            // mechanical tasks that never reach a clean cloud lane — the root
            // cause of #62 (observed on 00adb7e7 + 767afcc6, each reaped ~190s).
            let rp = repo_path.to_path_buf();
            let check = tokio::task::spawn_blocking(move || {
                Command::new(&program).args(&args).current_dir(&rp).output()
            })
            .await
            .map_err(|e| anyhow::anyhow!("verify subprocess task panicked: {e}"))?
            .with_context(|| format!("run {check_name} in {}", repo_path.display()))?;

            if !check.status.success() {
                let err = command_error(&check_name, &check);
                warn!(round, error = %err, "codegen edits failed verification");
                tokio::task::spawn_blocking(move || restore_snapshots(&snapshots))
                    .await
                    .map_err(|e| anyhow!("restore snapshots task panicked: {e}"))??;
                let rp = repo_path.to_path_buf();
                tokio::task::spawn_blocking(move || clean_worktree(&rp))
                    .await
                    .map_err(|e| anyhow!("clean worktree task panicked: {e}"))??;
                last_edits = Some(edit_summary);
                last_error = Some(err);
                continue;
            }
        } else {
            info!(
                round,
                repo = %repo_path.display(),
                "codegen post-apply verification skipped: no recognized verify command"
            );
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

/// Build a bounded repo-structure anchor for the codegen prompt: the crate/top-level layout
/// plus tracked source files whose path matches a task identifier. Prevents the model from
/// hallucinating non-existent paths. Returns None if `git ls-files` yields nothing.
fn repo_structure_context(repo_path: &Path, identifiers: &[String]) -> Option<String> {
    const MAX_CHARS: usize = 6_000;
    const MAX_RELEVANT_FILES: usize = 60;

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["ls-files"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let files: Vec<&str> = listing.lines().filter(|l| !l.is_empty()).collect();
    if files.is_empty() {
        return None;
    }

    // Top-level layout: the distinct crate roots (crates/<name>) + other top dirs.
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let parts: Vec<&str> = f.split('/').collect();
        let root = if parts.len() >= 2 && parts[0] == "crates" {
            format!("crates/{}", parts[1])
        } else if parts.len() >= 2 {
            parts[0].to_string()
        } else {
            f.to_string()
        };
        roots.insert(root);
    }

    // Files whose path matches any task identifier (case-insensitive) — the likely edit targets.
    let ids_lower: Vec<String> = identifiers.iter().map(|s| s.to_lowercase()).collect();
    let mut relevant: Vec<&str> = files
        .iter()
        .filter(|f| {
            let fl = f.to_lowercase();
            ids_lower.iter().any(|id| id.len() >= 3 && fl.contains(id.as_str()))
        })
        .copied()
        .take(MAX_RELEVANT_FILES)
        .collect();
    relevant.sort_unstable();

    let mut out = String::from("Top-level layout:\n");
    for r in &roots {
        out.push_str("  ");
        out.push_str(r);
        out.push('\n');
        if out.len() > MAX_CHARS {
            break;
        }
    }
    if !relevant.is_empty() {
        out.push_str("Tracked files relevant to this task:\n");
        for f in &relevant {
            out.push_str("  ");
            out.push_str(f);
            out.push('\n');
            if out.len() > MAX_CHARS {
                break;
            }
        }
    }
    Some(out)
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

    // Repo structure anchor: without a listing of what files ACTUALLY exist, the model invents
    // plausible-but-wrong paths (e.g. a Python `src/work_item/relations.py` in a Rust repo) and
    // every edit fails to apply. Inject the real crate layout + task-relevant tracked files so
    // SEARCH/REPLACE edits target files that exist. (build_prompt runs inside spawn_blocking.)
    if let Some(tree) = repo_structure_context(repo_path, &identifiers) {
        prompt.push_str("\n\nThis repository's actual structure (edit ONLY files that exist here; do not invent paths):\n");
        prompt.push_str(&tree);
    }

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

fn changed_crate_packages(repo_path: &Path, edits: &[Edit]) -> BTreeSet<String> {
    let mut package_cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut packages = BTreeSet::new();

    for edit in edits {
        let Some(rel) = normalize_relative_path(&edit.path) else {
            continue;
        };
        let Some(crate_dir) = crate_dir_for_rel_path(&rel) else {
            continue;
        };
        let package = package_cache
            .entry(crate_dir.clone())
            .or_insert_with(|| crate_package_for_path(repo_path, &rel));
        if let Some(package) = package {
            packages.insert(package.clone());
        }
    }

    packages
}

fn verify_command(repo_path: &Path, changed_crates: &[String]) -> Option<(String, Vec<String>)> {
    if repo_path.join("Cargo.toml").exists() {
        let mut args = vec!["check".to_string()];
        for package in changed_crates {
            args.push("-p".to_string());
            args.push(package.clone());
        }
        return Some(("cargo".to_string(), args));
    }

    let package_json = repo_path.join("package.json");
    if package_json.exists() {
        if package_json_has_script(&package_json, "typecheck") {
            return Some((
                "npm".to_string(),
                vec!["run".to_string(), "-s".to_string(), "typecheck".to_string()],
            ));
        }
        if repo_path.join("tsconfig.json").exists() {
            return Some((
                "npx".to_string(),
                vec!["-y".to_string(), "tsc".to_string(), "--noEmit".to_string()],
            ));
        }
        if package_json_has_script(&package_json, "build") {
            return Some((
                "npm".to_string(),
                vec!["run".to_string(), "-s".to_string(), "build".to_string()],
            ));
        }
    }

    None
}

fn package_json_has_script(package_json: &Path, script: &str) -> bool {
    let Ok(content) = fs::read_to_string(package_json) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    json.get("scripts")
        .and_then(|scripts| scripts.get(script))
        .is_some()
}

fn crate_package_for_path(repo_path: &Path, rel_path: &Path) -> Option<String> {
    let crate_dir = crate_dir_for_rel_path(rel_path)?;
    let manifest = repo_path.join(crate_dir).join("Cargo.toml");
    let content = fs::read_to_string(manifest).ok()?;
    package_name_from_manifest(&content)
}

fn crate_dir_for_rel_path(rel_path: &Path) -> Option<PathBuf> {
    let mut components = rel_path.components();
    match components.next()? {
        Component::Normal(part) if part == "crates" => {}
        _ => return None,
    }
    let Component::Normal(crate_name) = components.next()? else {
        return None;
    };

    Some(PathBuf::from("crates").join(crate_name))
}

fn package_name_from_manifest(content: &str) -> Option<String> {
    let mut in_package = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        return value
            .trim()
            .strip_prefix('"')?
            .split_once('"')
            .map(|(name, _)| name.to_string());
    }

    None
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

fn format_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{} {}", program, args.join(" "))
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
    fn codegen_resolves_crate_package_for_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("crates/ff-agent/Cargo.toml");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(
            &manifest,
            "[package]\nversion = \"0.1.0\"\nname = \"ff-agent\"\n",
        )
        .unwrap();

        assert_eq!(
            crate_package_for_path(dir.path(), Path::new("crates/ff-agent/src/foo.rs")),
            Some("ff-agent".to_string())
        );
    }

    #[test]
    fn codegen_verify_command_detects_project_type() {
        let rust_dir = tempfile::tempdir().unwrap();
        fs::write(rust_dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            verify_command(rust_dir.path(), &["ff-agent".to_string()]),
            Some((
                "cargo".to_string(),
                vec![
                    "check".to_string(),
                    "-p".to_string(),
                    "ff-agent".to_string()
                ]
            ))
        );

        let ts_dir = tempfile::tempdir().unwrap();
        fs::write(ts_dir.path().join("package.json"), "{\"scripts\":{}}\n").unwrap();
        fs::write(ts_dir.path().join("tsconfig.json"), "{}\n").unwrap();
        assert_eq!(
            verify_command(ts_dir.path(), &[]),
            Some((
                "npx".to_string(),
                vec!["-y".to_string(), "tsc".to_string(), "--noEmit".to_string()]
            ))
        );

        let empty_dir = tempfile::tempdir().unwrap();
        assert_eq!(verify_command(empty_dir.path(), &[]), None);
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
