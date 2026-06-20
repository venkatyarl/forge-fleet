use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
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

        let apply = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .arg("apply")
            .arg("--3way")
            .arg(&patch_path)
            .output()
            .with_context(|| format!("run git apply in {}", repo_path.display()))?;

        if !apply.status.success() {
            let err = command_error("git apply", &apply);
            warn!(round, error = %err, "codegen patch failed to apply");
            remove_patch_file(&patch_path);
            last_diff = Some(diff);
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
            warn!(round, error = %err, "codegen patch failed cargo check");
            let revert = Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .arg("checkout")
                .arg("--")
                .arg(".")
                .output()
                .with_context(|| {
                    format!("revert failed codegen patch in {}", repo_path.display())
                })?;
            if !revert.status.success() {
                return Err(anyhow!("{}", command_error("git checkout -- .", &revert)));
            }
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
