use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::PgPool;
use tracing::{info, warn};

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
    let patch_path = repo_path.join(".ff-codegen.patch");
    let mut last_diff: Option<String> = None;
    let mut last_error: Option<String> = None;
    let mut rounds = 0;

    for round in 1..=max_rounds {
        rounds = round;
        let prompt = build_prompt(task, last_diff.as_deref(), last_error.as_deref());
        info!(
            round,
            max_rounds, "requesting codegen diff from fleet model"
        );

        let response = crate::fleet_oneshot::fleet_oneshot(
            pool,
            &prompt,
            model_hint,
            Some(Duration::from_secs(300)),
        )
        .await
        .with_context(|| format!("fleet_oneshot round {round}"))?;

        let diff = match extract_diff_block(&response.text) {
            Some(diff) if !diff.trim().is_empty() => diff,
            _ => {
                let err =
                    "model response did not contain a non-empty fenced ```diff block".to_string();
                warn!(round, error = %err, "codegen response rejected");
                last_diff = None;
                last_error = Some(err);
                continue;
            }
        };

        fs::write(&patch_path, &diff)
            .with_context(|| format!("write patch {}", patch_path.display()))?;

        // Apply the diff with escalating tolerance — weak local coders produce
        // diffs with drifted @@ line numbers / context, which strict `git apply`
        // rejects. Try in order: plain git apply (atomic, no markers) → git apply
        // --recount (recomputes line numbers from context) → `patch --fuzz=3`
        // (ignores line numbers, fuzzy-matches context). None use --3way (which
        // would leave conflict markers on failure).
        match try_apply_patch(repo_path, &patch_path) {
            Ok(()) => {}
            Err(err) => {
                warn!(round, error = %err, "codegen patch failed to apply (all strategies)");
                clean_worktree(repo_path);
                remove_patch_file(&patch_path);
                last_diff = Some(diff);
                last_error = Some(err);
                continue;
            }
        }

        let check = Command::new("cargo")
            .arg("check")
            .current_dir(repo_path)
            .output()
            .with_context(|| format!("run cargo check in {}", repo_path.display()))?;

        if !check.status.success() {
            let err = command_error("cargo check", &check);
            warn!(round, error = %err, "codegen patch failed cargo check");
            // Revert the applied-but-broken patch (tracked edits + any new files).
            clean_worktree(repo_path);
            remove_patch_file(&patch_path);
            last_diff = Some(diff);
            last_error = Some(err);
            continue;
        }

        remove_patch_file(&patch_path);
        return Ok(CodegenOutcome {
            applied: true,
            rounds,
            final_diff: Some(diff),
            error: None,
        });
    }

    remove_patch_file(&patch_path);
    Ok(CodegenOutcome {
        applied: false,
        rounds,
        final_diff: None,
        error: last_error,
    })
}

fn build_prompt(task: &str, previous_diff: Option<&str>, previous_error: Option<&str>) -> String {
    let mut prompt = format!(
        "Task:\n{task}\n\n\
         Output ONLY a unified diff inside a single fenced code block tagged diff.\n\
         The diff must be in git-apply format, with paths relative to the repo root.\n\
         The target files ALREADY EXIST — produce a MODIFICATION diff against the existing\n\
         content (correct a/ and b/ paths, real @@ hunk headers with context lines). Do NOT\n\
         use /dev/null or new-file mode unless the file genuinely does not exist yet.\n\
         Do not include prose, explanations, markdown outside the single diff fence, or multiple code blocks.\n\
         Format exactly like:\n\
         ```diff\n\
         diff --git a/path b/path\n\
         --- a/path\n\
         +++ b/path\n\
         @@ ...\n\
         ```"
    );

    if let Some(diff) = previous_diff {
        prompt.push_str("\n\nPrevious diff that failed:\n```diff\n");
        prompt.push_str(diff.trim());
        prompt.push_str("\n```");
    }
    if let Some(error) = previous_error {
        prompt.push_str("\n\nExact failure to fix:\n```text\n");
        prompt.push_str(error.trim());
        prompt.push_str("\n```");
    }

    prompt
}

fn extract_diff_block(response: &str) -> Option<String> {
    let fence = "```";
    let mut offset = 0;
    while let Some(pos) = response[offset..].find(fence) {
        let open = offset + pos;
        let after_ticks = open + fence.len();
        let line_end_rel = response[after_ticks..].find('\n')?;
        let tag = response[after_ticks..after_ticks + line_end_rel].trim();
        let content_start = after_ticks + line_end_rel + 1;
        if tag.eq_ignore_ascii_case("diff") {
            let close_rel = response[content_start..].find(fence)?;
            return Some(response[content_start..content_start + close_rel].to_string());
        }
        offset = content_start;
    }
    None
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

fn remove_patch_file(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(path = %path.display(), error = %e, "failed to remove codegen patch file");
    }
}

/// Apply `patch_path` to `repo_path` with escalating tolerance. Returns Ok on
/// the first strategy that applies cleanly, else the combined error. Strategies,
/// least→most lenient: `git apply`, `git apply --recount`, `patch -p1 --fuzz=3`.
/// Never uses `--3way` (leaves conflict markers on failure).
fn try_apply_patch(repo_path: &Path, patch_path: &Path) -> std::result::Result<(), String> {
    let git_apply = |extra: &[&str]| -> std::result::Result<(), String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo_path).arg("apply");
        cmd.args(extra);
        cmd.arg(patch_path);
        match cmd.output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(command_error("git apply", &o)),
            Err(e) => Err(format!("git apply spawn failed: {e}")),
        }
    };

    let mut errs = Vec::new();
    match git_apply(&[]) {
        Ok(()) => return Ok(()),
        Err(e) => errs.push(e),
    }
    match git_apply(&["--recount"]) {
        Ok(()) => return Ok(()),
        Err(e) => errs.push(e),
    }
    // `patch` (BSD/GNU) fuzzy-matches context and ignores line numbers — the most
    // forgiving of weak-model diffs. -p1 strips the a/ b/ prefix.
    match Command::new("patch")
        .current_dir(repo_path)
        .args(["-p1", "--fuzz=3", "--no-backup-if-mismatch", "-i"])
        .arg(patch_path)
        .output()
    {
        Ok(o) if o.status.success() => return Ok(()),
        Ok(o) => errs.push(command_error("patch --fuzz", &o)),
        Err(e) => errs.push(format!("patch spawn failed: {e}")),
    }
    Err(errs.join(" | "))
}

/// Reset the worktree to HEAD so a failed or broken patch leaves NO residue for
/// the next round: `git checkout -- .` reverts tracked edits (incl. any conflict
/// markers a bad apply produced), `git clean -fd` removes new untracked files the
/// patch created. Best-effort — never aborts the loop.
fn clean_worktree(repo_path: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["checkout", "--", "."])
        .output();
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["clean", "-fd"])
        .output();
}
