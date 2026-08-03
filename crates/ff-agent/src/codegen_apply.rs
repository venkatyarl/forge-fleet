use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use sqlx::PgPool;
use tracing::{info, warn};

const SMALL_CONTEXT_FILE_BYTES: usize = 12_000;
const LARGE_CONTEXT_FILE_CHARS: usize = 16_000;
const REGION_CONTEXT_LINES: usize = 25;
const FALLBACK_LINES: usize = 60;
const CODEGEN_RUN_BUDGET: Duration = Duration::from_secs(420);
const CODEGEN_FINALIZE_RESERVE: Duration = Duration::from_secs(60);
const CODEGEN_CORRECTION_RESERVE: Duration = Duration::from_secs(45);
const CODEGEN_ROLLBACK_RESERVE: Duration = Duration::from_secs(5);
const CODEGEN_MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(210);
const CODEGEN_REQUEST_STARTUP: Duration = Duration::from_secs(15);
const CODEGEN_TELEMETRY_TIMEOUT: Duration = Duration::from_secs(2);
const CODEGEN_OUTPUT_TOKENS_PER_SEC: u64 = 8;
const CODEGEN_MIN_OUTPUT_TOKENS: u32 = 512;
const CODEGEN_MAX_OUTPUT_TOKENS: u32 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoundPlan {
    request_timeout: Duration,
    max_tokens: u32,
}

/// Admit a model round only when its request can fit before the absolute run
/// deadline while preserving time to parse/apply/verify and, when applicable,
/// one correction round. Token capacity is checked before clamping: otherwise
/// a 30-second generation window would be inflated to the 512-token minimum
/// even though the model cannot physically produce it at the conservative
/// throughput floor.
fn plan_round(remaining: Duration, rounds_after_current: u32) -> Option<RoundPlan> {
    let correction_reserve = if rounds_after_current > 0 {
        CODEGEN_CORRECTION_RESERVE
    } else {
        Duration::ZERO
    };
    let reserved = CODEGEN_FINALIZE_RESERVE.checked_add(correction_reserve)?;
    let available = remaining.checked_sub(reserved)?;
    let request_timeout = available.min(CODEGEN_MAX_REQUEST_TIMEOUT);
    let generation_window = request_timeout.checked_sub(CODEGEN_REQUEST_STARTUP)?;
    let achievable = generation_window
        .as_secs()
        .saturating_mul(CODEGEN_OUTPUT_TOKENS_PER_SEC);
    if achievable < u64::from(CODEGEN_MIN_OUTPUT_TOKENS) {
        return None;
    }

    Some(RoundPlan {
        request_timeout,
        max_tokens: achievable.min(u64::from(CODEGEN_MAX_OUTPUT_TOKENS)) as u32,
    })
}

async fn bounded_best_effort<F>(timeout: Duration, future: F) -> Option<F::Output>
where
    F: Future,
{
    tokio::time::timeout(timeout, future).await.ok()
}

fn round_diagnostic(round: u32, stage: &str, detail: impl std::fmt::Display) -> String {
    format!("round {round} {stage}: {detail}")
}

#[derive(Debug, Clone)]
pub struct CodegenOutcome {
    pub applied: bool,
    pub rounds: u32,
    pub final_diff: Option<String>,
    pub error: Option<String>,
    /// Stable catalog id of the local fleet model that produced the terminal response.
    pub builder_catalog_id: Option<String>,
    /// The model reported the task is ALREADY implemented / no change needed (it inspected the
    /// repo, often ran the tests, and produced no edits on purpose). The caller should mark the
    /// work_item done — NOT fail-retry — so an already-satisfied task drains instead of thrashing.
    pub already_done: bool,
}

/// Heuristic: a no-edit-blocks model response that AFFIRMATIVELY states the work is already
/// present / needs no change (as opposed to a confused/empty response). Deliberately requires a
/// completion phrase AND a no-change phrase to avoid false positives on genuine failures.
fn response_reports_already_done(text: &str) -> bool {
    // Explicit sentinel the prompt now asks for.
    if text
        .trim_start()
        .to_lowercase()
        .starts_with("already_implemented:")
    {
        return true;
    }
    let t = text.to_lowercase();
    let completion = [
        "already implemented",
        "already fully implemented",
        "already exists",
        "already present",
        "already landed",
        "no changes needed",
        "no change needed",
        "no changes are needed",
        "nothing to commit",
        "already done",
        "already satisfied",
    ];
    let corroborating = [
        "working tree",
        "tests pass",
        "test pass",
        "cargo check",
        "already an ancestor",
        "no edits",
        "no changes were made",
        "out of scope",
        "commit ",
    ];
    completion.iter().any(|p| t.contains(p)) && corroborating.iter().any(|p| t.contains(p))
}

/// One coder round as an `ff_interactions` row, tagged with the work item it
/// served (V250 episodic tagging) so the flat log replays as per-work-item
/// episodes. Pure record construction — the caller does the best-effort insert.
fn round_interaction(
    work_item_id: Option<uuid::Uuid>,
    round: u32,
    prompt: &str,
    resp: &crate::fleet_oneshot::FleetOneshot,
) -> ff_db::InteractionRecord {
    let engine = builder_engine(resp);
    let (tokens_in, tokens_out, tokens_estimated) = crate::llm_attribution::tokens_or_estimate(
        resp.tokens_in,
        resp.tokens_out,
        prompt,
        &resp.text,
    );
    let cost_usd = crate::llm_attribution::cost_usd(&engine, tokens_in, tokens_out);
    ff_db::InteractionRecord {
        channel: "codegen_apply".to_string(),
        request_text: prompt.chars().take(16000).collect(),
        request_meta: serde_json::json!({
            "round": round,
            "tokens_estimated": tokens_estimated,
            "route_provenance": resp.provenance.as_str(),
        }),
        route_decision: serde_json::json!({
            "deployment_id": resp.deployment_id,
            "endpoint": resp.endpoint,
            "worker_name": resp.worker_name,
            "catalog_id": resp.catalog_id,
            "model": resp.model,
            "provenance": resp.provenance.as_str(),
        }),
        engine: Some(engine),
        response_text: resp.text.chars().take(16000).collect(),
        tokens_in,
        tokens_out,
        cost_usd,
        latency_ms: i32::try_from(resp.latency_ms).ok(),
        outcome: "success".to_string(),
        worker_name: Some(resp.worker_name.clone()),
        endpoint: Some(resp.endpoint.clone()),
        work_item_id,
        purpose: Some("build".to_string()),
        ..Default::default()
    }
}

fn builder_engine(resp: &crate::fleet_oneshot::FleetOneshot) -> String {
    let label = resp
        .catalog_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!("local:{}", id.trim()))
        .unwrap_or_else(|| resp.model.clone());
    crate::llm_attribution::engine_label(&label)
}

/// SYSTEM-message contract for codegen (canary-3, 2026-07-29): reasoning
/// coders obey a system contract where they ignore mid-user-message format
/// instructions. Kept byte-identical in wording to the lab-proven thalia
/// experiments (first-try clean unified diffs / full files).
const CODEGEN_SYSTEM_CONTRACT: &str = "You are a code-editing engine, not a conversational assistant. \
Your ENTIRE reply must be SEARCH/REPLACE edit blocks in the exact format the user specifies — \
start the reply directly with the first '*** FILE:' block. NEVER explain, reason aloud, narrate, \
plan, or wrap the reply in markdown fences or prose. If no change is needed, reply with the single \
ALREADY_IMPLEMENTED line the user describes and nothing else.";

pub async fn codegen_apply(
    pool: &PgPool,
    repo_path: &Path,
    task: &str,
    model_hint: Option<&str>,
    max_rounds: u32,
    work_item_id: Option<uuid::Uuid>,
) -> Result<CodegenOutcome> {
    codegen_apply_with_target(
        pool,
        repo_path,
        task,
        model_hint,
        max_rounds,
        work_item_id,
        None,
    )
    .await
}

/// Run codegen with an optional explicit fleet target. The legacy entry point
/// above is deliberately just the unpinned wrapper so routing logic cannot
/// diverge between callers.
#[allow(clippy::too_many_arguments)]
pub async fn codegen_apply_with_target(
    pool: &PgPool,
    repo_path: &Path,
    task: &str,
    model_hint: Option<&str>,
    max_rounds: u32,
    work_item_id: Option<uuid::Uuid>,
    explicit_target: Option<&crate::fleet_oneshot::ResolvedFleetTarget>,
) -> Result<CodegenOutcome> {
    let run_deadline = tokio::time::Instant::now() + CODEGEN_RUN_BUDGET;
    let mut last_edits: Option<String> = None;
    let mut last_error: Option<String> = None;
    let mut diagnostics = Vec::new();
    let mut rounds = 0;

    for round in 1..=max_rounds {
        let rp = repo_path.to_path_buf();
        let task = task.to_string();
        let previous_edits = last_edits.clone();
        let previous_error = last_error.clone();
        let prompt_task = tokio::task::spawn_blocking(move || {
            build_prompt(
                &rp,
                &task,
                previous_edits.as_deref(),
                previous_error.as_deref(),
            )
        });
        let prompt = match tokio::time::timeout_at(run_deadline, prompt_task).await {
            Ok(Ok(result)) => result?,
            Ok(Err(error)) => return Err(anyhow!("build prompt task panicked: {error}")),
            Err(_) => {
                let diagnostic = round_diagnostic(
                    round,
                    "not started",
                    "building the grounded prompt exhausted the absolute run deadline",
                );
                warn!(round, error = %diagnostic, "codegen prompt build timed out");
                diagnostics.push(diagnostic.clone());
                last_error = Some(diagnostic);
                break;
            }
        };

        let remaining = run_deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(plan) = plan_round(remaining, max_rounds.saturating_sub(round)) else {
            let diagnostic = round_diagnostic(
                round,
                "not started",
                format!(
                    "remaining budget {remaining:?} cannot preserve finalization and a viable model request"
                ),
            );
            warn!(round, error = %diagnostic, "codegen round rejected by budget admission");
            diagnostics.push(diagnostic.clone());
            last_error = Some(diagnostic);
            break;
        };
        rounds = round;
        info!(
            round,
            max_rounds,
            request_timeout_ms = plan.request_timeout.as_millis(),
            max_tokens = plan.max_tokens,
            "requesting codegen edits from fleet model"
        );

        // Constrain to code-capable deployments: a build must never route to a
        // non-coder model (Lucy-1.7B / SmolVLM2-video), which return prose and no
        // valid diff → "no diff to check" failures. Capability-based via the
        // router, not a model list; fails closed if no coder is healthy.
        //
        // Ctx floor (2026-07-29, canary-2 root cause): glm-4.5-air is a REASONING
        // model — its slot must hold prompt + think + max_tokens or the reply
        // truncates into prose. Estimate prompt tokens (chars/4) and require the
        // slot to fit it plus the output plus a think reserve, so fat
        // repo-context prompts route to the 32K slots instead of thalia's 12K.
        let est_prompt_tokens = i32::try_from(prompt.len() / 4).unwrap_or(i32::MAX);
        let max_tokens = i32::try_from(plan.max_tokens).unwrap_or(i32::MAX);
        let min_ctx = Some(
            est_prompt_tokens
                .saturating_add(max_tokens)
                .saturating_add(2048)
                .min(49152),
        );
        // Canary-3 root cause: with the format contract mid-user-message, the
        // reasoning model thinks out loud in `content` and burns the completion
        // budget before any edit block. Strict SYSTEM contract (lab-proven on
        // thalia) plus the admitted completion budget keeps the reply usable.
        let request = crate::fleet_oneshot::fleet_oneshot_for_ctx_with_target(
            pool,
            &prompt,
            model_hint,
            Some(plan.request_timeout),
            Some("code"),
            min_ctx,
            Some(CODEGEN_SYSTEM_CONTRACT),
            plan.max_tokens,
            explicit_target,
        );
        let response = match tokio::time::timeout(plan.request_timeout, request).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let diagnostic =
                    round_diagnostic(round, "model request failed", format!("{error:#}"));
                warn!(round, error = %diagnostic, "codegen model request failed");
                diagnostics.push(diagnostic.clone());
                last_edits = None;
                last_error = Some(diagnostic);
                if explicit_target.is_some() {
                    break;
                }
                continue;
            }
            Err(_) => {
                let diagnostic = round_diagnostic(
                    round,
                    "model request timed out",
                    format!("after {:?}", plan.request_timeout),
                );
                warn!(round, error = %diagnostic, "codegen model request timed out");
                diagnostics.push(diagnostic.clone());
                last_edits = None;
                last_error = Some(diagnostic);
                if explicit_target.is_some() {
                    break;
                }
                continue;
            }
        };

        // Per-round episode capture (V250 episodic tagging): `fleet_oneshot`
        // never inserts into `ff_interactions` itself — it returns the
        // attribution for THIS caller to log. Without this, local-lane coder
        // turns only exist as the dispatch-level summary row, which can't
        // attribute individual rounds. Best-effort — never fails codegen.
        let rec = round_interaction(work_item_id, round, &prompt, &response);
        let telemetry_timeout = CODEGEN_TELEMETRY_TIMEOUT
            .min(run_deadline.saturating_duration_since(tokio::time::Instant::now()));
        if !telemetry_timeout.is_zero() {
            match bounded_best_effort(telemetry_timeout, ff_db::pg_record_interaction(pool, &rec))
                .await
            {
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    warn!(round, error = %error, "codegen: interaction capture failed (non-fatal)");
                }
                None => {
                    warn!(
                        round,
                        timeout_ms = telemetry_timeout.as_millis(),
                        "codegen: interaction capture timed out (non-fatal)"
                    );
                }
            }
        }

        let edits = match parse_edit_blocks(&response.text) {
            Ok(edits) if !edits.is_empty() => edits,
            Ok(_) => {
                // No edit blocks. If the model affirmatively reports the task is ALREADY done
                // (feature exists, tests pass, nothing to commit), that's a legitimate terminal
                // success — mark done so the caller drains it, instead of retrying to no end.
                if response_reports_already_done(&response.text) {
                    info!(
                        round,
                        "codegen: model reports task already implemented — no changes needed (marking done)"
                    );
                    return Ok(CodegenOutcome {
                        applied: false,
                        rounds,
                        final_diff: None,
                        error: None,
                        builder_catalog_id: response.catalog_id.clone(),
                        already_done: true,
                    });
                }
                let diagnostic = round_diagnostic(
                    round,
                    "response rejected",
                    "model response contained NO edit blocks — it was prose. Reply with ONLY edit blocks starting with '*** FILE:' — no explanation, no reasoning, no markdown fences around the whole reply",
                );
                warn!(round, error = %diagnostic, "codegen response rejected");
                diagnostics.push(diagnostic.clone());
                last_edits = None;
                last_error = Some(diagnostic);
                continue;
            }
            Err(e) => {
                let diagnostic = round_diagnostic(round, "response rejected", e);
                warn!(round, error = %diagnostic, "codegen response rejected");
                diagnostics.push(diagnostic.clone());
                last_edits = Some(response.text);
                last_error = Some(diagnostic);
                continue;
            }
        };
        let edit_summary = format_edit_summary(&edits);

        // File mutation is deliberately synchronous and bounded to the model's
        // touched paths. Tokio task cancellation is cooperative, so no future
        // can be dropped between the first write and construction of the
        // rollback guard. Running this in a detached spawn_blocking task would
        // let the caller observe/commit transient edits before that task later
        // returned its guard and rolled them back.
        let transaction = match apply_edits(repo_path, &edits).map(EditTransaction::new) {
            Ok(transaction) => transaction,
            Err(e) => {
                let diagnostic = round_diagnostic(round, "edits rejected", e);
                warn!(round, error = %diagnostic, "codegen edits failed to apply");
                diagnostics.push(diagnostic.clone());
                last_edits = Some(edit_summary);
                last_error = Some(diagnostic);
                continue;
            }
        };

        // Guard against no-op edits: a SEARCH/REPLACE where REPLACE == the matched
        // text (or edits that otherwise change nothing) would pass apply + cargo
        // check and be reported applied:true while the working tree is UNCHANGED
        // (live-observed false-success on a 183K file). Compare only the touched
        // paths so unrelated pre-existing dirt cannot create a false success.
        let transaction = match transaction.has_changes() {
            Ok(true) => transaction,
            Ok(false) => {
                let diagnostic = round_diagnostic(
                    round,
                    "edits rejected",
                    "applied edits produced NO touched-file change (no-op SEARCH/REPLACE)",
                );
                warn!(round, error = %diagnostic, "codegen edits were a no-op");
                diagnostics.push(diagnostic.clone());
                last_edits = Some(edit_summary);
                last_error = Some(diagnostic);
                continue;
            }
            Err(error) => {
                let diagnostic = round_diagnostic(round, "edit inspection failed", error);
                warn!(round, error = %diagnostic, "codegen could not inspect touched files");
                diagnostics.push(diagnostic.clone());
                last_edits = Some(edit_summary);
                last_error = Some(diagnostic);
                continue;
            }
        };

        if tokio::time::Instant::now() >= run_deadline {
            let diagnostic = round_diagnostic(
                round,
                "edits rejected",
                "parsing and applying edits exhausted the absolute run deadline",
            );
            warn!(round, error = %diagnostic, "codegen apply exceeded deadline");
            diagnostics.push(diagnostic.clone());
            last_edits = Some(edit_summary);
            last_error = Some(diagnostic);
            continue;
        }

        let changed_packages = changed_crate_packages(repo_path, &edits)
            .into_iter()
            .collect::<Vec<_>>();
        let verify = verify_command(repo_path, &changed_packages);
        if let Some((program, args)) = verify {
            let check_name = format_command(&program, &args);
            let remaining = run_deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(verify_timeout) = remaining.checked_sub(CODEGEN_ROLLBACK_RESERVE) else {
                let diagnostic = round_diagnostic(
                    round,
                    "verification not started",
                    format!(
                        "remaining budget {remaining:?} does not preserve rollback reserve {CODEGEN_ROLLBACK_RESERVE:?}"
                    ),
                );
                warn!(round, error = %diagnostic, "codegen verification rejected by budget");
                diagnostics.push(diagnostic.clone());
                last_edits = Some(edit_summary);
                last_error = Some(diagnostic);
                continue;
            };

            match run_verify_command(repo_path, &program, &args, verify_timeout).await {
                Ok(check) if check.status.success() => {}
                Ok(check) => {
                    let diagnostic = round_diagnostic(
                        round,
                        "verification failed",
                        command_error(&check_name, &check),
                    );
                    warn!(round, error = %diagnostic, "codegen edits failed verification");
                    diagnostics.push(diagnostic.clone());
                    last_edits = Some(edit_summary);
                    last_error = Some(diagnostic);
                    continue;
                }
                Err(error) => {
                    let diagnostic = round_diagnostic(round, "verification failed", error);
                    warn!(round, error = %diagnostic, "codegen verification could not complete");
                    diagnostics.push(diagnostic.clone());
                    last_edits = Some(edit_summary);
                    last_error = Some(diagnostic);
                    continue;
                }
            }
        } else {
            info!(
                round,
                repo = %repo_path.display(),
                "codegen post-apply verification skipped: no recognized verify command"
            );
        }

        transaction.commit();
        return Ok(CodegenOutcome {
            applied: true,
            rounds,
            final_diff: Some(edit_summary),
            error: None,
            builder_catalog_id: response.catalog_id.clone(),
            already_done: false,
        });
    }

    Ok(CodegenOutcome {
        applied: false,
        rounds,
        final_diff: None,
        error: if diagnostics.is_empty() {
            last_error
        } else {
            Some(diagnostics.join("\n"))
        },
        builder_catalog_id: None,
        already_done: false,
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

/// Owns the exact pre-edit contents of every touched file. Unless committed,
/// dropping the transaction restores those contents and removes files created
/// by the model. Keeping this guard alive across verification makes timeout,
/// task cancellation, and ordinary retry paths preserve both unrelated files
/// and pre-existing dirty content in touched files.
#[derive(Debug)]
struct EditTransaction {
    snapshots: Option<Vec<FileSnapshot>>,
}

impl EditTransaction {
    fn new(snapshots: Vec<FileSnapshot>) -> Self {
        Self {
            snapshots: Some(snapshots),
        }
    }

    fn has_changes(&self) -> Result<bool> {
        let Some(snapshots) = self.snapshots.as_deref() else {
            return Ok(false);
        };
        for snapshot in snapshots {
            match &snapshot.previous {
                Some(previous) => {
                    let current = fs::read_to_string(&snapshot.path).with_context(|| {
                        format!("read touched file {}", snapshot.path.display())
                    })?;
                    if &current != previous {
                        return Ok(true);
                    }
                }
                None if snapshot.path.exists() => return Ok(true),
                None => {}
            }
        }
        Ok(false)
    }

    fn commit(mut self) {
        self.snapshots = None;
    }
}

impl Drop for EditTransaction {
    fn drop(&mut self) {
        let Some(snapshots) = self.snapshots.take() else {
            return;
        };
        if let Err(error) = restore_snapshots(&snapshots) {
            warn!(error = %error, "failed to roll back codegen edit transaction");
        }
    }
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
            ids_lower
                .iter()
                .any(|id| id.len() >= 3 && fl.contains(id.as_str()))
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
         - Paths must be relative to the repo root.\n\
         - You do NOT have a shell or file access. You CANNOT edit files directly. Do not say you \
           'made the edits', 'left them uncommitted', or 'ran the tests' — you MUST emit the edit \
           blocks as your entire response; the harness applies them.\n\
         - If NO change is needed because the task is already implemented, reply with exactly the \
           single line: ALREADY_IMPLEMENTED: <one-sentence reason>.\n\n\
         Worked example — a valid response for 'add a greeting fn to src/util.rs':\n\
         *** FILE: src/util.rs\n\
         <<<<<<< SEARCH\n\
         =======\n\
         pub fn greeting(name: &str) -> String {{\n\
             format!(\"hello, {{name}}\")\n\
         }}\n\
         >>>>>>> REPLACE"
    );

    let identifiers = task_identifiers(task);
    // Grounding flag for region extraction: test tasks must always see the
    // file's #[cfg(test)] module (2026-07-29 hallucination root cause).
    let wants_test = task.to_ascii_lowercase().contains("test");

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
                &extract_relevant_regions(&content, &identifiers, wants_test),
            ));
        }
    }

    // Repo structure anchor: without a listing of what files ACTUALLY exist, the model invents
    // plausible-but-wrong paths (e.g. a Python `src/work_item/relations.py` in a Rust repo) and
    // every edit fails to apply. Keep grounded source regions first, then add lower-value
    // repository listings as path guardrails. (build_prompt runs inside spawn_blocking.)
    if let Some(tree) = repo_structure_context(repo_path, &identifiers) {
        prompt.push_str("\n\nThis repository's actual structure (edit ONLY files that exist here; do not invent paths):\n");
        prompt.push_str(&tree);
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
        "test",
        "tests",
        "testing",
        "unit",
        "integration",
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

fn extract_relevant_regions(content: &str, identifiers: &[String], wants_test: bool) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    if lines.is_empty() {
        return "No content.\n".to_string();
    }

    let test_spans = cfg_test_line_spans(&lines);
    let mut implementation_ranges: Vec<(usize, usize)> = Vec::new();
    let mut test_ranges: Vec<(usize, usize)> = Vec::new();
    if !identifiers.is_empty() {
        for (idx, line) in lines.iter().enumerate() {
            let lower = line.to_ascii_lowercase();
            if identifiers
                .iter()
                .any(|identifier| lower.contains(identifier))
            {
                let start = idx.saturating_sub(REGION_CONTEXT_LINES);
                let end = (idx + REGION_CONTEXT_LINES).min(lines.len() - 1);
                if line_in_spans(idx, &test_spans) {
                    push_merged_range(&mut test_ranges, (start, end));
                } else {
                    push_merged_range(&mut implementation_ranges, (start, end));
                }
            }
        }
    }

    // Test intent still benefits from cfg(test) grounding, but every module is
    // bounded independently. Never treat "first #[cfg(test)] through EOF" as
    // tests: large Rust files commonly interleave helpers, nested modules, and
    // multiple unrelated test modules.
    if wants_test {
        for &(start, end) in &test_spans {
            let bounded_end = end.min(start + REGION_CONTEXT_LINES);
            push_merged_range(&mut test_ranges, (start, bounded_end));
        }
    }

    // An exact named implementation and an exact named test/helper are the
    // two highest-value regions for an edit model. Put those ranges first in
    // their respective partitions so earlier incidental substring matches
    // cannot evict either one from a fixed context budget.
    if let Some(range) = named_function_range(&lines, &test_spans, identifiers, false) {
        implementation_ranges.insert(0, range);
    }
    if let Some(range) = named_function_range(&lines, &test_spans, identifiers, true) {
        test_ranges.insert(0, range);
    }
    prioritize_named_function_range(&mut implementation_ranges, &lines, identifiers, false);
    prioritize_named_function_range(&mut test_ranges, &lines, identifiers, true);

    let mut implementation = Vec::new();
    for range in implementation_ranges {
        push_non_overlapping_range(&mut implementation, range);
    }
    let mut tests = Vec::new();
    for range in test_ranges {
        push_non_overlapping_range(&mut tests, range);
    }

    if implementation.is_empty() && tests.is_empty() {
        return fallback_head_tail_regions(&lines);
    }

    if wants_test && !implementation.is_empty() && !tests.is_empty() {
        return render_partitioned_ranges(&lines, &implementation, &tests);
    }

    implementation.extend(tests);
    render_ranges(&lines, &implementation)
}

fn named_function_range(
    lines: &[&str],
    test_spans: &[(usize, usize)],
    identifiers: &[String],
    test_function: bool,
) -> Option<(usize, usize)> {
    lines.iter().enumerate().find_map(|(idx, line)| {
        if line_in_spans(idx, test_spans) != test_function {
            return None;
        }
        let matched = identifiers.iter().any(|identifier| {
            identifier.starts_with("test_") == test_function
                && line_contains_named_function(line, identifier)
        });
        matched.then(|| {
            (
                idx.saturating_sub(REGION_CONTEXT_LINES),
                (idx + REGION_CONTEXT_LINES).min(lines.len() - 1),
            )
        })
    })
}

fn prioritize_named_function_range(
    ranges: &mut Vec<(usize, usize)>,
    lines: &[&str],
    identifiers: &[String],
    test_function: bool,
) {
    let Some(priority_idx) = ranges.iter().position(|(start, end)| {
        lines[*start..=*end].iter().any(|line| {
            identifiers.iter().any(|identifier| {
                identifier.starts_with("test_") == test_function
                    && line_contains_named_function(line, identifier)
            })
        })
    }) else {
        return;
    };

    ranges.swap(0, priority_idx);
}

fn line_contains_named_function(line: &str, identifier: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let needle = format!("fn {identifier}");
    lower.match_indices(&needle).any(|(start, _)| {
        lower
            .as_bytes()
            .get(start + needle.len())
            .is_some_and(|next| matches!(next, b'(' | b'<' | b' ' | b'\t'))
    })
}

fn line_in_spans(line: usize, spans: &[(usize, usize)]) -> bool {
    spans
        .iter()
        .any(|(start, end)| *start <= line && line <= *end)
}

fn push_merged_range(ranges: &mut Vec<(usize, usize)>, range: (usize, usize)) {
    if let Some((_, last_end)) = ranges.last_mut()
        && range.0 <= *last_end + 1
    {
        *last_end = (*last_end).max(range.1);
        return;
    }
    ranges.push(range);
}

fn push_non_overlapping_range(ranges: &mut Vec<(usize, usize)>, range: (usize, usize)) {
    let mut pending = vec![range];
    for &(existing_start, existing_end) in ranges.iter() {
        let mut next = Vec::new();
        for (start, end) in pending {
            if end < existing_start || existing_end < start {
                next.push((start, end));
                continue;
            }
            if start < existing_start {
                next.push((start, existing_start - 1));
            }
            if existing_end < end {
                next.push((existing_end + 1, end));
            }
        }
        pending = next;
        if pending.is_empty() {
            return;
        }
    }
    ranges.extend(pending);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustLexMode {
    Code,
    BlockComment(usize),
    String,
    RawString(usize),
}

#[derive(Debug, Clone, Copy)]
struct RustBraceScanner {
    mode: RustLexMode,
}

impl Default for RustBraceScanner {
    fn default() -> Self {
        Self {
            mode: RustLexMode::Code,
        }
    }
}

impl RustBraceScanner {
    fn is_code(self) -> bool {
        self.mode == RustLexMode::Code
    }

    fn scan_line(&mut self, line: &str, mut on_brace: impl FnMut(u8)) {
        let bytes = line.as_bytes();
        let mut idx = 0usize;

        while idx < bytes.len() {
            match self.mode {
                RustLexMode::Code => {
                    if bytes[idx..].starts_with(b"//") {
                        return;
                    }
                    if bytes[idx..].starts_with(b"/*") {
                        self.mode = RustLexMode::BlockComment(1);
                        idx += 2;
                        continue;
                    }
                    if let Some((hashes, content_start)) = raw_string_open(bytes, idx) {
                        self.mode = RustLexMode::RawString(hashes);
                        idx = content_start;
                        continue;
                    }
                    if literal_prefix_boundary(bytes, idx) && bytes[idx..].starts_with(b"b\"") {
                        self.mode = RustLexMode::String;
                        idx += 2;
                        continue;
                    }
                    if bytes[idx] == b'"' {
                        self.mode = RustLexMode::String;
                        idx += 1;
                        continue;
                    }
                    if literal_prefix_boundary(bytes, idx)
                        && bytes[idx..].starts_with(b"b'")
                        && let Some(end) = char_literal_end(line, idx + 1)
                    {
                        idx = end;
                        continue;
                    }
                    if bytes[idx] == b'\''
                        && let Some(end) = char_literal_end(line, idx)
                    {
                        idx = end;
                        continue;
                    }
                    if matches!(bytes[idx], b'{' | b'}') {
                        on_brace(bytes[idx]);
                    }
                    idx += 1;
                }
                RustLexMode::BlockComment(mut depth) => {
                    if bytes[idx..].starts_with(b"/*") {
                        depth += 1;
                        self.mode = RustLexMode::BlockComment(depth);
                        idx += 2;
                    } else if bytes[idx..].starts_with(b"*/") {
                        depth -= 1;
                        self.mode = if depth == 0 {
                            RustLexMode::Code
                        } else {
                            RustLexMode::BlockComment(depth)
                        };
                        idx += 2;
                    } else {
                        idx += 1;
                    }
                }
                RustLexMode::String => match bytes[idx] {
                    b'\\' => idx = (idx + 2).min(bytes.len()),
                    b'"' => {
                        self.mode = RustLexMode::Code;
                        idx += 1;
                    }
                    _ => idx += 1,
                },
                RustLexMode::RawString(hashes) => {
                    if bytes[idx] == b'"'
                        && bytes
                            .get(idx + 1..idx + 1 + hashes)
                            .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                    {
                        self.mode = RustLexMode::Code;
                        idx += 1 + hashes;
                    } else {
                        idx += 1;
                    }
                }
            }
        }
    }
}

fn literal_prefix_boundary(bytes: &[u8], idx: usize) -> bool {
    idx == 0
        || !matches!(bytes[idx - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
            && bytes[idx - 1].is_ascii()
}

/// Return `(hash_count, content_start)` for a Rust `r#"..."#` or
/// `br#"..."#` opener beginning at `idx`.
fn raw_string_open(bytes: &[u8], idx: usize) -> Option<(usize, usize)> {
    if !literal_prefix_boundary(bytes, idx) {
        return None;
    }

    let mut cursor = if bytes[idx..].starts_with(b"br") {
        idx + 2
    } else if bytes[idx] == b'r' {
        idx + 1
    } else {
        return None;
    };
    let hash_start = cursor;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    Some((cursor - hash_start, cursor + 1))
}

/// Confirm a complete Rust character literal rather than treating every
/// apostrophe (including lifetimes and labels) as a character delimiter.
fn char_literal_end(line: &str, quote: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let content = quote.checked_add(1)?;
    let first = *bytes.get(content)?;

    let close = if first == b'\\' {
        match *bytes.get(content + 1)? {
            b'x' => {
                let digits = bytes.get(content + 2..content + 4)?;
                if !digits.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                let value =
                    (digits[0] as char).to_digit(16)? * 16 + (digits[1] as char).to_digit(16)?;
                if value > 0x7f {
                    return None;
                }
                content + 4
            }
            b'u' if bytes.get(content + 2) == Some(&b'{') => {
                let mut cursor = content + 3;
                let mut digits = 0usize;
                let mut value = 0u32;
                while let Some(byte) = bytes.get(cursor) {
                    match byte {
                        b'}' if digits > 0 && char::from_u32(value).is_some() => break,
                        b'_' => {}
                        byte if byte.is_ascii_hexdigit() && digits < 6 => {
                            value = value.checked_mul(16)? + (*byte as char).to_digit(16)?;
                            digits += 1;
                        }
                        _ => return None,
                    }
                    cursor += 1;
                }
                if bytes.get(cursor) != Some(&b'}') {
                    return None;
                }
                cursor + 1
            }
            b'n' | b'r' | b't' | b'0' | b'\\' | b'\'' | b'"' => content + 2,
            _ => return None,
        }
    } else {
        let character = line.get(content..)?.chars().next()?;
        if matches!(character, '\'' | '\n' | '\r') {
            return None;
        }
        content + character.len_utf8()
    };

    (bytes.get(close) == Some(&b'\'')).then_some(close + 1)
}

fn cfg_test_attribute_lines(lines: &[&str]) -> Vec<usize> {
    let mut scanner = RustBraceScanner::default();
    let mut attributes = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if scanner.is_code() && line.trim_start().starts_with("#[cfg(test)]") {
            attributes.push(idx);
        }
        scanner.scan_line(line, |_| {});
    }
    attributes
}

fn cfg_test_line_spans(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();

    for start in cfg_test_attribute_lines(lines) {
        if spans.last().is_some_and(|(_, end)| start <= *end) {
            continue;
        }

        let mut scanner = RustBraceScanner::default();
        let mut end = start;
        let mut depth = 0isize;
        let mut opened = false;

        while end < lines.len() {
            scanner.scan_line(lines[end], |brace| match brace {
                b'{' => {
                    depth += 1;
                    opened = true;
                }
                b'}' if opened => depth -= 1,
                _ => {}
            });
            if opened && depth <= 0 {
                break;
            }
            end += 1;
        }

        if end >= lines.len() {
            end = (start + REGION_CONTEXT_LINES).min(lines.len() - 1);
        }
        spans.push((start, end));
    }

    spans
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
    render_ranges_capped(lines, ranges, LARGE_CONTEXT_FILE_CHARS)
}

fn render_partitioned_ranges(
    lines: &[&str],
    implementation: &[(usize, usize)],
    tests: &[(usize, usize)],
) -> String {
    let implementation_budget = LARGE_CONTEXT_FILE_CHARS / 2;
    let mut out = render_ranges_capped(lines, implementation, implementation_budget);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let test_budget = LARGE_CONTEXT_FILE_CHARS.saturating_sub(char_count(&out));
    let rendered_tests = render_ranges_capped(lines, tests, test_budget);
    push_str_capped(&mut out, &rendered_tests, LARGE_CONTEXT_FILE_CHARS);
    out
}

fn render_ranges_capped(lines: &[&str], ranges: &[(usize, usize)], cap: usize) -> String {
    let mut out = String::new();

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

        if char_count(&out) + char_count(&block) > cap {
            render_truncated_range(lines, ranges, idx, *start, *end, cap, &mut out);
            break;
        }
        out.push_str(&block);
    }

    out
}

fn render_truncated_range(
    lines: &[&str],
    ranges: &[(usize, usize)],
    idx: usize,
    start: usize,
    end: usize,
    cap: usize,
    out: &mut String,
) {
    let omitted = ranges.len() - idx;
    let notice = format!(
        "\n... omitted {omitted} later/truncated region(s) after ~{cap} chars. \
         Those lines are INVISIBLE to you — do NOT invent their content; SEARCH only from lines shown above.\n"
    );
    let reserve = char_count(&notice);
    if char_count(out) + reserve >= cap {
        return;
    }

    if idx > 0 {
        push_str_capped(out, "\n", cap - reserve);
    }
    push_str_capped(
        out,
        &format!("Region (lines {}-{}):\n", start + 1, end + 1),
        cap - reserve,
    );
    for line in &lines[start..=end] {
        if char_count(out) + char_count(line) + reserve <= cap {
            out.push_str(line);
        } else {
            push_str_capped(out, line, cap - reserve);
            break;
        }
    }
    if !out.ends_with('\n') && char_count(out) + reserve < cap {
        out.push('\n');
    }
    if char_count(out) + reserve <= cap {
        out.push_str(&notice);
    }
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn push_str_capped(out: &mut String, text: &str, cap: usize) {
    let remaining = cap.saturating_sub(char_count(out));
    if remaining == 0 {
        return;
    }
    out.extend(text.chars().take(remaining));
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
    let mut blocks = response.split("*** FILE:");
    let prefix = blocks.next().unwrap_or_default();
    if !prefix.trim().is_empty() {
        return Err(anyhow!(
            "model response has leading text before the first *** FILE: marker"
        ));
    }
    for raw_block in blocks {
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

/// Whitespace-tolerant search: find the byte span in `content` whose lines match
/// `search`'s lines after trimming each line's leading/trailing whitespace and
/// ignoring fully-blank lines. Returns `(start_byte, len_bytes)` of the matched
/// span in the ORIGINAL content (so the real bytes are replaced), or None.
/// Conservative — requires ALL non-blank search lines to match consecutively.
fn fuzzy_find_block(content: &str, search: &str) -> Option<(usize, usize)> {
    let needle: Vec<&str> = search
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if needle.is_empty() {
        return None;
    }
    // Byte offset of the start of each content line.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let lines: Vec<&str> = content.lines().collect();
    // Slide over content lines; at each start, match needle skipping blank
    // content lines too.
    for start in 0..lines.len() {
        let mut ci = start; // content line index
        let mut ni = 0; // needle index
        let mut last_matched = start;
        while ni < needle.len() && ci < lines.len() {
            let cl = lines[ci].trim();
            if cl.is_empty() {
                ci += 1;
                continue; // skip blank content lines
            }
            if cl == needle[ni] {
                last_matched = ci;
                ci += 1;
                ni += 1;
            } else {
                break;
            }
        }
        if ni == needle.len() {
            let start_byte = line_starts[start];
            // End byte = end of the last matched line (include its trailing \n if present).
            let end_byte = if last_matched + 1 < line_starts.len() {
                line_starts[last_matched + 1]
            } else {
                content.len()
            };
            return Some((start_byte, end_byte - start_byte));
        }
    }
    None
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
    // Exact match first; if that fails, a WHITESPACE-TOLERANT match (operator
    // 2026-07-26). Local coders (devstral) and cloud CLIs frequently emit SEARCH
    // blocks that differ only in leading/trailing whitespace or blank lines —
    // an exact `content.find` then fails with "SEARCH block not found", the whole
    // build produces no diff, and the item churns 4 rounds × many attempts (the
    // #1 completion-rate killer). The fuzzy fallback matches the search block's
    // non-whitespace content line-by-line and replaces the real byte span, so a
    // trivially-misindented edit lands instead of failing the build.
    let (pos, matched_len) = match content.find(&edit.search) {
        Some(pos) => (pos, edit.search.len()),
        None => match fuzzy_find_block(&content, &edit.search) {
            Some((pos, len)) => {
                info!(path = %edit.path, "codegen: SEARCH matched via whitespace-tolerant fallback");
                (pos, len)
            }
            None => {
                // Quote the mismatch back: a bare "not found" lets the model
                // repeat the same invented block for every round (2026-07-29).
                let head: String = edit.search.lines().take(3).collect::<Vec<_>>().join("\n");
                return Err(anyhow!(
                    "SEARCH block not found in {}. The SEARCH text must be copied VERBATIM \
                     from the file content shown in the prompt — do NOT invent lines, helpers, \
                     or modules (e.g. no 'test_utils' unless shown). Your SEARCH block began:\n\
                     {}\n\
                     If the exact lines you need were NOT shown in the prompt, do not guess — \
                     pick SEARCH text from lines that WERE shown.",
                    edit.path,
                    head
                ));
            }
        },
    };
    let mut updated = String::with_capacity(content.len() - matched_len + edit.replace.len());
    updated.push_str(&content[..pos]);
    updated.push_str(&edit.replace);
    updated.push_str(&content[pos + matched_len..]);
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
    let mut first_error: Option<anyhow::Error> = None;
    for snapshot in snapshots.iter().rev() {
        let result = match &snapshot.previous {
            Some(content) => fs::write(&snapshot.path, content)
                .with_context(|| format!("restore {}", snapshot.path.display())),
            None => match fs::remove_file(&snapshot.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("remove {}", snapshot.path.display())),
            },
        };
        if let Err(error) = result {
            warn!(path = %snapshot.path.display(), error = %error, "codegen snapshot restore failed; continuing with remaining paths");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
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

async fn run_verify_command(
    repo_path: &Path,
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<std::process::Output> {
    let check_name = format_command(program, args);
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let child = command
        .spawn()
        .with_context(|| format!("spawn {check_name} in {}", repo_path.display()))?;
    let mut process_group = VerifyProcessGroup::new(child.id());
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{check_name} timed out after {timeout:?}; process group killed"))?
        .with_context(|| format!("wait for {check_name} in {}", repo_path.display()))?;
    process_group.disarm();
    Ok(output)
}

/// Cancellation guard for verification commands. `kill_on_drop` covers only
/// the immediate child; Cargo/npm can leave descendants holding output pipes
/// and CPU after their leader dies. The child is its own process-group leader,
/// so dropping this guard on timeout or task cancellation kills the full tree.
#[derive(Debug)]
struct VerifyProcessGroup {
    pid: Option<u32>,
}

impl VerifyProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for VerifyProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            crate::task_runner::kill_process_group(pid);
        }
    }
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
    fn round_interaction_carries_episodic_tags() {
        let work_item_id = uuid::Uuid::new_v4();
        let resp = crate::fleet_oneshot::FleetOneshot {
            text: "*** FILE: src/lib.rs".to_string(),
            deployment_id: uuid::Uuid::new_v4(),
            endpoint: "http://192.168.5.103:55000".to_string(),
            worker_name: "worker-a".to_string(),
            catalog_id: Some("glm-4.5-air".to_string()),
            model: "qwen3-coder-30b".to_string(),
            provenance: crate::fleet_oneshot::ResolvedTargetProvenance::ExplicitCatalog,
            latency_ms: 1234,
            tokens_in: 100,
            tokens_out: 50,
        };

        let rec = round_interaction(Some(work_item_id), 2, "do the task", &resp);

        assert_eq!(rec.channel, "codegen_apply");
        assert_eq!(rec.work_item_id, Some(work_item_id));
        assert_eq!(rec.purpose.as_deref(), Some("build"));
        assert_eq!(rec.request_meta["round"], 2);
        assert_eq!(rec.request_meta["route_provenance"], "explicit_catalog");
        assert_eq!(
            rec.route_decision["deployment_id"],
            resp.deployment_id.to_string()
        );
        assert_eq!(rec.route_decision["provenance"], "explicit_catalog");
        assert_eq!(rec.worker_name.as_deref(), Some("worker-a"));
        assert_eq!(rec.endpoint.as_deref(), Some("http://192.168.5.103:55000"));
        assert_eq!(rec.engine.as_deref(), Some("local:glm-4.5-air"));
        assert_eq!(rec.latency_ms, Some(1234));

        // No work item in scope (e.g. `ff codegen` from the CLI) → the row
        // still lands, just untagged.
        let rec = round_interaction(None, 1, "do the task", &resp);
        assert_eq!(rec.work_item_id, None);
        assert_eq!(rec.purpose.as_deref(), Some("build"));
    }

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

        let regions = extract_relevant_regions(&content, &["special_handler".to_string()], false);

        assert!(regions.contains("Region (lines 15-65):"));
        assert!(regions.contains("fn special_handler() {}\n"));
        assert!(!regions.contains("No task identifiers matched"));
    }
    #[test]
    fn codegen_regions_keep_implementation_before_bounded_tests() {
        // Large file: identifier hit early, tests module at the end. Test
        // grounding must not bury the exact implementation region behind a
        // lower-value cfg(test) module.
        let mut content = String::new();
        for i in 1..40 {
            content.push_str(&format!("fn filler_{i}() {{}}\n"));
        }
        content.push_str("fn special_handler() {}\n");
        content.push_str("#[cfg(test)]\nmod tests {\n    fn real_helper() {}\n}\n");

        let regions = extract_relevant_regions(&content, &["special_handler".to_string()], true);
        let tests_pos = regions.find("#[cfg(test)]").expect("tests module shown");
        let handler_pos = regions
            .find("fn special_handler")
            .expect("identifier shown");
        assert!(
            handler_pos < tests_pos,
            "implementation region must precede bounded test grounding"
        );
        assert!(regions.contains("fn real_helper()"));
    }

    #[test]
    fn codegen_extract_regions_preserves_exact_impl_and_named_test_under_cap() {
        let content = include_str!("../../ff-db/src/queries.rs");
        let identifiers = task_identifiers(
            "add tests for offload_workload_for_kind and test_offload_workload_for_kind",
        );
        let regions = extract_relevant_regions(content, &identifiers, true);

        let impl_pos = regions
            .find("fn offload_workload_for_kind(kind: Option<&str>) -> Option<&'static str>")
            .expect("byte-exact real implementation signature shown");
        let test_pos = regions
            .find("fn test_offload_workload_for_kind")
            .expect("byte-exact real test signature shown");
        assert!(
            impl_pos < test_pos,
            "implementation should be grounded before named test/helper"
        );
        assert!(
            !regions.contains("fn deployment_health_fresh"),
            "the first unrelated cfg(test) item must not become an unbounded test tail"
        );
        assert!(
            char_count(&regions) <= LARGE_CONTEXT_FILE_CHARS,
            "regions exceeded declared cap: {}",
            char_count(&regions)
        );
    }

    #[test]
    fn codegen_exact_impl_and_test_have_reserved_budget_after_many_earlier_hits() {
        let mut content = String::new();
        for idx in 0..900 {
            content.push_str(&format!(
                "const OFFLOAD_WORKLOAD_FOR_KIND_REFERENCE_{idx}: &str = \"noise\";\n"
            ));
        }
        content.push_str(
            "fn offload_workload_for_kind(kind: Option<&str>) -> Option<&'static str> {\n    kind.map(|_| \"code-gen\")\n}\n",
        );
        content.push_str("#[cfg(test)]\nmod noisy_tests {\n");
        for idx in 0..900 {
            content.push_str(&format!(
                "    fn offload_workload_for_kind_noise_{idx}() {{}}\n"
            ));
        }
        content.push_str("}\n#[cfg(test)]\nmod exact_tests {\n");
        content.push_str("    fn test_offload_workload_for_kind() {}\n}\n");

        let identifiers =
            task_identifiers("repair offload_workload_for_kind and test_offload_workload_for_kind");
        let regions = extract_relevant_regions(&content, &identifiers, true);

        let impl_pos = regions
            .find("fn offload_workload_for_kind(kind: Option<&str>)")
            .expect("reserved implementation region");
        let test_pos = regions
            .find("fn test_offload_workload_for_kind()")
            .expect("reserved exact test region");
        assert!(impl_pos < test_pos);
        assert!(char_count(&regions) <= LARGE_CONTEXT_FILE_CHARS);
    }

    #[test]
    fn codegen_task_identifiers_drop_generic_test_words_not_exact_symbols() {
        let identifiers =
            task_identifiers("unit testing for tests test_offload_workload_for_kind integration");

        assert!(!identifiers.iter().any(|id| id == "unit"));
        assert!(!identifiers.iter().any(|id| id == "testing"));
        assert!(!identifiers.iter().any(|id| id == "tests"));
        assert!(!identifiers.iter().any(|id| id == "integration"));
        assert!(
            identifiers
                .iter()
                .any(|id| id == "test_offload_workload_for_kind")
        );
    }

    #[test]
    fn codegen_named_function_anchor_requires_identifier_boundary() {
        assert!(line_contains_named_function(
            "pub async fn target_name<T>() {}",
            "target_name"
        ));
        assert!(!line_contains_named_function(
            "fn target_name_suffix() {}",
            "target_name"
        ));
    }

    #[test]
    fn codegen_char_literal_recognizer_validates_unicode_and_ignores_lifetimes() {
        assert_eq!(char_literal_end(r#"'\u{1f600}'"#, 0), Some(11));
        assert_eq!(char_literal_end("'😀'", 0), Some(6));
        assert_eq!(char_literal_end(r#"'\u{d800}'"#, 0), None);
        assert_eq!(char_literal_end("'static", 0), None);
        assert_eq!(char_literal_end("'outer:", 0), None);
    }

    #[test]
    fn codegen_render_ranges_keeps_source_for_oversized_first_region() {
        let huge_line = format!("fn target() {{ /* {} */ }}\n", "x".repeat(40_000));
        let lines = vec![huge_line.as_str()];

        let regions = render_ranges(&lines, &[(0, 0)]);

        assert!(regions.contains("Region (lines 1-1):"));
        assert!(regions.contains("fn target()"));
        assert!(regions.contains("omitted"));
        assert!(char_count(&regions) <= LARGE_CONTEXT_FILE_CHARS);
    }

    #[test]
    fn codegen_render_ranges_preserves_multiline_boundaries_before_notice() {
        let owned = (0..400)
            .map(|idx| format!("let value_{idx} = \"{}\";\n", "x".repeat(80)))
            .collect::<Vec<_>>();
        let lines = owned.iter().map(String::as_str).collect::<Vec<_>>();

        let regions = render_ranges(&lines, &[(0, lines.len() - 1)]);

        assert!(regions.contains(&format!(
            "let value_0 = \"{}\";\nlet value_1 =",
            "x".repeat(80)
        )));
        assert!(regions.contains("\n... omitted"));
        assert!(char_count(&regions) <= LARGE_CONTEXT_FILE_CHARS);
    }

    #[test]
    fn codegen_cfg_test_spans_ignore_comments_strings_chars_and_lifetimes() {
        let lines = [
            "const IGNORED: &str = r##\"",
            "#[cfg(test)]",
            "mod fake { }",
            "\"##;",
            "/* outer {",
            "   /* nested } */",
            "#[cfg(test)]",
            "*/",
            "#[cfg(test)]",
            "mod real_tests {",
            "    let normal = \"{ }\\\"still string\";",
            "    let bytes = b\"{ }\";",
            "    let raw = r###\"{ }\"###;",
            "    let byte_raw = br##\"{ }\"##;",
            "    let open = '{';",
            "    let quote = '\\'';",
            "    let value: &'static str = \"}\";",
            "    'outer: loop { break 'outer; }",
            "    // } ignored",
            "    /* nested comment {",
            "       /* } */",
            "       } still comment */",
            "}",
        ];

        assert_eq!(cfg_test_line_spans(&lines), vec![(8, 22)]);
    }

    #[test]
    fn codegen_cfg_test_spans_handle_multiple_items_and_unclosed_fallback() {
        let lines = [
            "#[cfg(test)]",
            "mod one {",
            "    let unicode = '😀';",
            "}",
            "#[cfg(test)]",
            "fn two() { let escaped = '\\u{1f600}'; }",
            "#[cfg(test)]",
            "mod unclosed {",
            "    let raw = r#\"} never closes;",
        ];

        assert_eq!(cfg_test_line_spans(&lines), vec![(0, 3), (4, 5), (6, 8)]);
    }

    #[test]
    fn codegen_apply_error_quotes_search_head_and_forbids_invention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "real content\n").unwrap();

        let err = apply_edits(
            dir.path(),
            &[Edit {
                path: "src/lib.rs".to_string(),
                search: "invented line one\ninvented line two\ninvented line three\ninvented line four\n".to_string(),
                replace: "whatever\n".to_string(),
            }],
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("invented line one"),
            "quotes SEARCH head: {err}"
        );
        assert!(err.contains("VERBATIM"), "explains the contract: {err}");
        assert!(err.contains("do NOT invent"), "forbids invention: {err}");
    }

    #[test]
    fn codegen_extract_relevant_regions_falls_back_to_head_and_tail() {
        let content = (1..=140)
            .map(|line| format!("let unrelated_{line} = {line};\n"))
            .collect::<String>();

        let regions =
            extract_relevant_regions(&content, &["missing_identifier".to_string()], false);

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

    #[test]
    fn codegen_round_plan_preserves_finalize_and_correction_reserves() {
        assert_eq!(
            plan_round(Duration::from_secs(420), 2),
            Some(RoundPlan {
                request_timeout: Duration::from_secs(210),
                max_tokens: 1560,
            })
        );
        assert_eq!(
            plan_round(Duration::from_secs(165), 0),
            Some(RoundPlan {
                request_timeout: Duration::from_secs(105),
                max_tokens: 720,
            })
        );
        assert_eq!(plan_round(Duration::from_secs(105), 0), None);
        assert_eq!(plan_round(Duration::from_secs(104), 0), None);
    }

    #[test]
    fn codegen_two_slow_rounds_do_not_admit_a_third() {
        let mut remaining = CODEGEN_RUN_BUDGET;
        let first = plan_round(remaining, 2).expect("first round admitted");
        remaining = remaining.saturating_sub(first.request_timeout);
        let second = plan_round(remaining, 1).expect("correction round admitted");
        remaining = remaining.saturating_sub(second.request_timeout);

        assert_eq!(first.request_timeout, Duration::from_secs(210));
        assert_eq!(second.request_timeout, Duration::from_secs(105));
        assert_eq!(plan_round(remaining, 0), None);
    }

    #[tokio::test(start_paused = true)]
    async fn codegen_pending_telemetry_is_bounded() {
        let result =
            bounded_best_effort(CODEGEN_TELEMETRY_TIMEOUT, std::future::pending::<()>()).await;

        assert!(result.is_none());
    }

    #[test]
    fn codegen_literal_nit_tail_is_explicitly_rejected_without_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old\n").unwrap();
        let response = "*** FILE: src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n<nit: 1 line(s) skipped...>";

        let error = parse_edit_blocks(response).unwrap_err();
        let diagnostic = round_diagnostic(1, "response rejected", error);

        assert!(diagnostic.contains("round 1 response rejected"));
        assert!(diagnostic.contains("trailing text after REPLACE marker"));
        assert_eq!(fs::read_to_string(path).unwrap(), "old\n");
    }

    #[test]
    fn codegen_leading_prose_or_fence_is_explicitly_rejected_without_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old\n").unwrap();
        let valid = "*** FILE: src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE";

        for response in [
            format!("I will make the change.\n{valid}"),
            format!("```rust\n{valid}"),
        ] {
            let error = parse_edit_blocks(&response).unwrap_err();
            assert!(
                error.to_string().contains("leading text before the first"),
                "unexpected rejection: {error}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        }
    }

    #[test]
    fn codegen_valid_edit_transaction_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old\n").unwrap();
        let edits = parse_edit_blocks(
            "*** FILE: src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE",
        )
        .unwrap();

        let transaction = EditTransaction::new(apply_edits(dir.path(), &edits).unwrap());
        assert!(transaction.has_changes().unwrap());
        transaction.commit();

        assert_eq!(fs::read_to_string(path).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn codegen_cancellation_restores_only_touched_paths() {
        let dir = tempfile::tempdir().unwrap();
        let touched = dir.path().join("src/lib.rs");
        let created = dir.path().join("src/generated.rs");
        let unrelated = dir.path().join("notes.txt");
        fs::create_dir_all(touched.parent().unwrap()).unwrap();
        fs::write(&touched, "pre-existing dirty content\n").unwrap();
        fs::write(&unrelated, "unrelated dirty content\n").unwrap();
        let edits = vec![
            Edit {
                path: "src/lib.rs".to_string(),
                search: "pre-existing dirty content\n".to_string(),
                replace: "model content\n".to_string(),
            },
            Edit {
                path: "src/generated.rs".to_string(),
                search: String::new(),
                replace: "generated\n".to_string(),
            },
        ];
        let transaction = EditTransaction::new(apply_edits(dir.path(), &edits).unwrap());
        let task = tokio::spawn(async move {
            let _transaction = transaction;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        assert_eq!(
            fs::read_to_string(touched).unwrap(),
            "pre-existing dirty content\n"
        );
        assert!(!created.exists());
        assert_eq!(
            fs::read_to_string(unrelated).unwrap(),
            "unrelated dirty content\n"
        );
    }

    #[test]
    fn codegen_restore_continues_after_one_path_fails() {
        let dir = tempfile::tempdir().unwrap();
        let restorable = dir.path().join("restorable.txt");
        let invalid_target = dir.path().join("is-a-directory");
        fs::write(&restorable, "model content\n").unwrap();
        fs::create_dir(&invalid_target).unwrap();
        let snapshots = vec![
            FileSnapshot {
                path: restorable.clone(),
                previous: Some("original content\n".to_string()),
            },
            FileSnapshot {
                path: invalid_target,
                previous: Some("cannot write a directory\n".to_string()),
            },
        ];

        let error = restore_snapshots(&snapshots).unwrap_err();

        assert!(error.to_string().contains("restore"));
        assert_eq!(
            fs::read_to_string(restorable).unwrap(),
            "original content\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codegen_verification_cancellation_restores_touched_file() {
        let dir = tempfile::tempdir().unwrap();
        let touched = dir.path().join("src/lib.rs");
        let started = dir.path().join("verification-started");
        fs::create_dir_all(touched.parent().unwrap()).unwrap();
        fs::write(&touched, "original\n").unwrap();
        let transaction = EditTransaction::new(
            apply_edits(
                dir.path(),
                &[Edit {
                    path: "src/lib.rs".to_string(),
                    search: "original\n".to_string(),
                    replace: "model change\n".to_string(),
                }],
            )
            .unwrap(),
        );
        let repo_path = dir.path().to_path_buf();
        let script = format!("touch {}; sleep 30", started.display());
        let task = tokio::spawn(async move {
            let _transaction = transaction;
            let _ = run_verify_command(
                &repo_path,
                "sh",
                &["-c".to_string(), script],
                Duration::from_secs(60),
            )
            .await;
        });
        for _ in 0..100 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.exists(), "verification process did not start");

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(fs::read_to_string(touched).unwrap(), "original\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codegen_verification_timeout_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let error = run_verify_command(
            dir.path(),
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            Duration::from_millis(25),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("process group killed"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
